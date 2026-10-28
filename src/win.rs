//! Petits appels Win32 dont winit n'a pas l'équivalent : opacité globale de la
//! fenêtre (fenêtre « layered »), position du curseur, zone de travail de l'écran.

use windows::Win32::Foundation::{COLORREF, HWND, POINT, RECT};
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Console::{FreeConsole, GetConsoleProcessList};
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use std::sync::atomic::{AtomicBool, Ordering};

use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

#[derive(Clone, Copy)]
pub struct Hwnd(HWND);

impl Hwnd {
    pub fn of(window: &impl HasWindowHandle) -> Option<Self> {
        match window.window_handle().ok()?.as_raw() {
            RawWindowHandle::Win32(h) => Some(Self(HWND(h.hwnd.get() as *mut _))),
            _ => None,
        }
    }

    /// Opacité globale de la fenêtre. Le style WS_EX_LAYERED est réappliqué à chaque
    /// fois car winit réécrit les styles étendus (plein écran, premier plan, clic à travers…).
    pub fn set_opacity(self, opacity: f32) {
        let alpha = (opacity.clamp(0.0, 1.0) * 255.0).round() as u8;
        unsafe {
            let ex = GetWindowLongPtrW(self.0, GWL_EXSTYLE);
            if ex & WS_EX_LAYERED.0 as isize == 0 {
                SetWindowLongPtrW(self.0, GWL_EXSTYLE, ex | WS_EX_LAYERED.0 as isize);
            }
            let _ = SetLayeredWindowAttributes(self.0, COLORREF(0), alpha, LWA_ALPHA);
        }
    }

    /// Le curseur est-il au-dessus de la fenêtre ? Fonctionne même quand la fenêtre
    /// laisse passer les clics (elle ne reçoit alors plus aucun évènement souris).
    pub fn cursor_inside(self) -> bool {
        unsafe {
            let mut p = POINT::default();
            let mut r = RECT::default();
            if GetCursorPos(&mut p).is_err() || GetWindowRect(self.0, &mut r).is_err() {
                return false;
            }
            p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom
        }
    }

    /// Corrige le passage d'un écran à l'autre quand leurs échelles diffèrent (ex. 100 %
    /// et 150 %). Windows propose dans WM_DPICHANGED un rectangle calculé pour que la
    /// fenêtre reste sous le curseur ; winit l'ignore et recalcule sa propre taille, la
    /// fenêtre grossit, repasse en majorité sur l'écran de départ et rebondit.
    /// On laisse winit traiter le message (mise à l'échelle du contenu), puis on applique
    /// le rectangle proposé par Windows.
    pub fn follow_suggested_dpi_rect(self) {
        unsafe {
            let _ = SetWindowSubclass(self.0, Some(dpi_subclass_proc), 0x5049_5050, 0);
        }
    }

    /// Raccourci global Ctrl+Alt+K (mode contrôle), reçu même quand la fenêtre n'a pas
    /// le focus. Nécessite `follow_suggested_dpi_rect` (même sous-classement de fenêtre).
    pub fn register_control_hotkey(self) -> bool {
        unsafe { RegisterHotKey(Some(self.0), HOTKEY_CONTROL_ID, MOD_CONTROL | MOD_ALT | MOD_NOREPEAT, u32::from(b'K')).is_ok() }
    }

    /// Zone de travail (écran moins la barre des tâches) de l'écran qui contient la fenêtre.
    pub fn work_area(self) -> Option<(i32, i32, u32, u32)> {
        unsafe {
            let mon = MonitorFromWindow(self.0, MONITOR_DEFAULTTONEAREST);
            let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
            if !GetMonitorInfoW(mon, &mut info).as_bool() {
                return None;
            }
            let r = info.rcWork;
            Some((r.left, r.top, (r.right - r.left) as u32, (r.bottom - r.top) as u32))
        }
    }
}

/// Touche Alt enfoncée (état global, même si la fenêtre n'a pas le focus).
pub fn alt_down() -> bool {
    unsafe { GetAsyncKeyState(VK_MENU.0 as i32) as u16 & 0x8000 != 0 }
}

/// Lancé par double-clic, Windows ouvre une console vide à côté de l'interface :
/// on la ferme si elle n'appartient qu'à nous (pas si on a été lancé depuis un terminal).
pub fn detach_own_console() {
    unsafe {
        let mut pids = [0u32; 2];
        if GetConsoleProcessList(&mut pids) == 1 {
            let _ = FreeConsole();
        }
    }
}

/// Autorise cet exécutable dans le pare-feu Windows (entrant, tous profils réseau).
/// Supprime d'abord les règles existantes pour l'exe : quand on n'autorise que les
/// « réseaux privés » dans l'invite de Windows, il ajoute des règles de BLOCAGE pour
/// les réseaux publics, qui l'emportent sur toute autorisation.
/// Demande l'élévation (UAC) et attend la fin ; renvoie une erreur si refusé ou échoué.
pub fn allow_in_firewall() -> anyhow::Result<()> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject};
    use windows::Win32::UI::Shell::{SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
    use windows::core::{HSTRING, w};

    let exe = std::env::current_exe()?;
    let exe = exe.display();
    let params = HSTRING::from(format!(
        "/c netsh advfirewall firewall delete rule name=all program=\"{exe}\" & \
         netsh advfirewall firewall add rule name=\"PiP-PbP\" dir=in action=allow program=\"{exe}\" enable=yes profile=any"
    ));
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
        lpVerb: w!("runas"),
        lpFile: w!("cmd.exe"),
        lpParameters: windows::core::PCWSTR(params.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    unsafe {
        ShellExecuteExW(&mut info).map_err(|_| anyhow::anyhow!("autorisation administrateur refusée"))?;
        let mut code = 1u32;
        WaitForSingleObject(info.hProcess, INFINITE);
        let _ = GetExitCodeProcess(info.hProcess, &mut code);
        let _ = CloseHandle(info.hProcess);
        if code != 0 {
            anyhow::bail!("netsh a échoué (code {code})");
        }
    }
    Ok(())
}

unsafe extern "system" fn dpi_subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    unsafe {
        if msg == WM_HOTKEY && wparam.0 as i32 == HOTKEY_CONTROL_ID {
            HOTKEY_PRESSED.store(true, Ordering::Relaxed);
            return LRESULT(0);
        }
        if msg != WM_DPICHANGED {
            return DefSubclassProc(hwnd, msg, wparam, lparam);
        }
        let r = *(lparam.0 as *const RECT);
        let result = DefSubclassProc(hwnd, msg, wparam, lparam);
        let _ = SetWindowPos(
            hwnd,
            None,
            r.left,
            r.top,
            r.right - r.left,
            r.bottom - r.top,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
        result
    }
}

const HOTKEY_CONTROL_ID: i32 = 0x5050;
static HOTKEY_PRESSED: AtomicBool = AtomicBool::new(false);

/// Renvoie `true` (une seule fois) si Ctrl+Alt+K a été pressé depuis le dernier appel.
pub fn take_control_hotkey() -> bool {
    HOTKEY_PRESSED.swap(false, Ordering::Relaxed)
}

/// Coordonnées en pixels physiques, y compris pour SendInput (écrans à 150 %, etc.).
pub fn make_dpi_aware() {
    use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

/// Reproduit sur ce PC la souris et le clavier envoyés par le viewer, sur l'écran partagé.
pub struct Injector {
    /// Écran partagé en pixels physiques : x, y, largeur, hauteur.
    monitor: (i32, i32, i32, i32),
}

impl Injector {
    pub fn new(monitor: (i32, i32, i32, i32)) -> Self {
        Self { monitor }
    }

    /// Position absolue, `x` et `y` de 0 à 65535 sur l'écran partagé.
    pub fn move_to(&self, x: u16, y: u16) {
        let (mx, my, mw, mh) = self.monitor;
        let px = mx + (x as i64 * (mw - 1).max(0) as i64 / 65535) as i32;
        let py = my + (y as i64 * (mh - 1).max(0) as i64 / 65535) as i32;
        // SendInput absolu : coordonnées normalisées sur tout le bureau virtuel.
        let (vx, vy, vw, vh) = unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN).max(2),
                GetSystemMetrics(SM_CYVIRTUALSCREEN).max(2),
            )
        };
        let nx = ((px - vx) as i64 * 65535 / (vw - 1) as i64) as i32;
        let ny = ((py - vy) as i64 * 65535 / (vh - 1) as i64) as i32;
        send_mouse(MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK, nx, ny, 0);
    }

    pub fn button(&self, button: u8, down: bool) {
        let (flags, data) = match (button, down) {
            (0, true) => (MOUSEEVENTF_LEFTDOWN, 0),
            (0, false) => (MOUSEEVENTF_LEFTUP, 0),
            (1, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
            (1, false) => (MOUSEEVENTF_RIGHTUP, 0),
            (2, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
            (2, false) => (MOUSEEVENTF_MIDDLEUP, 0),
            (3 | 4, true) => (MOUSEEVENTF_XDOWN, if button == 3 { 1 } else { 2 }),
            (3 | 4, false) => (MOUSEEVENTF_XUP, if button == 3 { 1 } else { 2 }),
            _ => return,
        };
        send_mouse(flags, 0, 0, data);
    }

    pub fn wheel(&self, vertical: i16, horizontal: i16) {
        if vertical != 0 {
            send_mouse(MOUSEEVENTF_WHEEL, 0, 0, vertical as i32 as u32);
        }
        if horizontal != 0 {
            send_mouse(MOUSEEVENTF_HWHEEL, 0, 0, horizontal as i32 as u32);
        }
    }

    /// Touche par scancode (indépendant de la disposition du clavier) ; 0xE0xx = touche étendue.
    pub fn key(&self, scancode: u16, down: bool) {
        let mut flags = KEYEVENTF_SCANCODE;
        if scancode & 0xFF00 == 0xE000 {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        if !down {
            flags |= KEYEVENTF_KEYUP;
        }
        let input = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VIRTUAL_KEY(0), wScan: scancode & 0xFF, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
        };
        unsafe {
            SendInput(&[input], size_of::<INPUT>() as i32);
        }
    }
}

fn send_mouse(flags: MOUSE_EVENT_FLAGS, dx: i32, dy: i32, data: u32) {
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy, mouseData: data, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    };
    unsafe {
        SendInput(&[input], size_of::<INPUT>() as i32);
    }
}
