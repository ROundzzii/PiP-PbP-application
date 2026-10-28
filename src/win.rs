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
    pub fn move_to(&self, x: u16, y: u16) -> bool {
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
        send_mouse(MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK, nx, ny, 0)
    }

    pub fn button(&self, button: u8, down: bool) -> bool {
        let (flags, data) = match (button, down) {
            (0, true) => (MOUSEEVENTF_LEFTDOWN, 0),
            (0, false) => (MOUSEEVENTF_LEFTUP, 0),
            (1, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
            (1, false) => (MOUSEEVENTF_RIGHTUP, 0),
            (2, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
            (2, false) => (MOUSEEVENTF_MIDDLEUP, 0),
            (3 | 4, true) => (MOUSEEVENTF_XDOWN, if button == 3 { 1 } else { 2 }),
            (3 | 4, false) => (MOUSEEVENTF_XUP, if button == 3 { 1 } else { 2 }),
            _ => return false,
        };
        send_mouse(flags, 0, 0, data)
    }

    pub fn wheel(&self, vertical: i16, horizontal: i16) -> bool {
        let mut ok = true;
        if vertical != 0 {
            ok &= send_mouse(MOUSEEVENTF_WHEEL, 0, 0, vertical as i32 as u32);
        }
        if horizontal != 0 {
            ok &= send_mouse(MOUSEEVENTF_HWHEEL, 0, 0, horizontal as i32 as u32);
        }
        ok
    }

    /// Touche par scancode (indépendant de la disposition du clavier) ; 0xE0xx = touche étendue.
    pub fn key(&self, scancode: u16, down: bool) -> bool {
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
        unsafe { SendInput(&[input], size_of::<INPUT>() as i32) == 1 }
    }
}

/// `false` si Windows a refusé l'entrée (ex. fenêtre au premier plan lancée en administrateur).
fn send_mouse(flags: MOUSE_EVENT_FLAGS, dx: i32, dy: i32, data: u32) -> bool {
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy, mouseData: data, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    };
    unsafe { SendInput(&[input], size_of::<INPUT>() as i32) == 1 }
}

/// Reçoit chaque touche capturée : (scancode, enfoncée). 0xE0xx = touche étendue.
type KeySink = Box<dyn Fn(u16, bool) + Send>;

static KEY_SINK: std::sync::Mutex<Option<KeySink>> = std::sync::Mutex::new(None);
/// Identifiant du thread qui porte le hook clavier (0 = pas de hook).
static KEY_HOOK_THREAD: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static CTRL_DOWN: AtomicBool = AtomicBool::new(false);
static HOOK_SEEN: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static HOOK_INJECTED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Diagnostic : touches vues par le hook depuis le début du contrôle, dont simulées.
pub fn keyboard_hook_stats() -> (u32, u32) {
    (HOOK_SEEN.load(Ordering::Relaxed), HOOK_INJECTED.load(Ordering::Relaxed))
}
static ALT_DOWN: AtomicBool = AtomicBool::new(false);

/// Mode contrôle : intercepte TOUT le clavier avant Windows (touche Windows, Alt+Tab,
/// Ctrl+Échap…), l'envoie à `sink` et l'empêche d'agir sur ce PC. Ctrl+Alt+K reste
/// géré localement (rendre la main). Ctrl+Alt+Suppr ne peut pas être intercepté.
///
/// Le hook tourne sur son propre thread avec sa propre boucle de messages : Windows
/// n'appelle un hook bas niveau que pendant que son thread attend des messages, et le
/// supprime en silence s'il répond trop lentement (la boucle de la fenêtre ne suffit pas).
pub fn start_keyboard_capture(sink: KeySink) -> bool {
    *KEY_SINK.lock().unwrap() = Some(sink);
    CTRL_DOWN.store(false, Ordering::Relaxed);
    ALT_DOWN.store(false, Ordering::Relaxed);
    HOOK_SEEN.store(0, Ordering::Relaxed);
    HOOK_INJECTED.store(0, Ordering::Relaxed);
    if KEY_HOOK_THREAD.load(Ordering::Relaxed) != 0 {
        return true;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || unsafe {
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::System::Threading::GetCurrentThreadId;
        let module = GetModuleHandleW(None).ok().map(|m| windows::Win32::Foundation::HINSTANCE(m.0));
        let hook = match SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook_proc), module, 0) {
            Ok(h) => h,
            Err(_) => {
                let _ = tx.send(false);
                return;
            }
        };
        KEY_HOOK_THREAD.store(GetCurrentThreadId(), Ordering::Relaxed);
        let _ = tx.send(true);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {}
        let _ = UnhookWindowsHookEx(hook);
    });
    rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap_or(false)
}

pub fn stop_keyboard_capture() {
    *KEY_SINK.lock().unwrap() = None;
    let tid = KEY_HOOK_THREAD.swap(0, Ordering::Relaxed);
    if tid != 0 {
        unsafe {
            let _ = PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0));
        }
    }
}

unsafe extern "system" fn keyboard_hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if code >= 0 {
            let k = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
            HOOK_SEEN.fetch_add(1, Ordering::Relaxed);
            // Les claviers « gaming » (logiciels de macros, etc.) réinjectent souvent les
            // frappes : elles sont marquées simulées mais viennent bien de l'utilisateur.
            if k.flags.contains(LLKHF_INJECTED) {
                HOOK_INJECTED.fetch_add(1, Ordering::Relaxed);
            }
            {
                let down = !k.flags.contains(LLKHF_UP);
                let vk = k.vkCode as u16;
                if vk == VK_LCONTROL.0 || vk == VK_RCONTROL.0 || vk == VK_CONTROL.0 {
                    CTRL_DOWN.store(down, Ordering::Relaxed);
                }
                if vk == VK_LMENU.0 || vk == VK_RMENU.0 || vk == VK_MENU.0 {
                    ALT_DOWN.store(down, Ordering::Relaxed);
                }
                if down && vk == u16::from(b'K') && CTRL_DOWN.load(Ordering::Relaxed) && ALT_DOWN.load(Ordering::Relaxed) {
                    HOTKEY_PRESSED.store(true, Ordering::Relaxed);
                    return LRESULT(1);
                }
                let mut sc = (k.scanCode & 0xFF) as u16;
                if k.flags.contains(LLKHF_EXTENDED) {
                    sc |= 0xE000;
                }
                if let Some(sink) = KEY_SINK.lock().unwrap().as_ref() {
                    sink(sc, down);
                    return LRESULT(1);
                }
            }
        }
        CallNextHookEx(None, code, wparam, lparam)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU32;

    /// Vérifie que le hook clavier reçoit bien les touches. Simule F24 (touche absente
    /// des claviers, interceptée et bloquée par le hook). Lancer avec `--ignored`.
    #[test]
    #[ignore]
    fn keyboard_hook_receives_keys() {
        let got = Arc::new(AtomicU32::new(0));
        let g = got.clone();
        assert!(start_keyboard_capture(Box::new(move |_, _| {
            g.fetch_add(1, Ordering::Relaxed);
        })));
        std::thread::sleep(std::time::Duration::from_millis(200));
        for up in [false, true] {
            let mut flags = KEYBD_EVENT_FLAGS(0);
            if up {
                flags |= KEYEVENTF_KEYUP;
            }
            let input = INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VK_F24, wScan: 0, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
            };
            unsafe { SendInput(&[input], size_of::<INPUT>() as i32) };
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
        stop_keyboard_capture();
        let (seen, injected) = keyboard_hook_stats();
        println!("touches transmises : {}, vues par le hook : {seen} (simulées : {injected})", got.load(Ordering::Relaxed));
        assert_eq!(got.load(Ordering::Relaxed), 2);
    }
}
