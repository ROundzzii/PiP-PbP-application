//! Petits appels Win32 dont winit n'a pas l'équivalent : opacité globale de la
//! fenêtre (fenêtre « layered »), position du curseur, zone de travail de l'écran.

use windows::Win32::Foundation::{COLORREF, HWND, POINT, RECT};
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Console::{FreeConsole, GetConsoleProcessList};
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

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

    pub fn raw(self) -> isize {
        self.0.0 as isize
    }

    /// Petite notification en haut au centre de la fenêtre, qui disparaît seule.
    pub fn notify(self, text: &str) {
        let mut r = RECT::default();
        unsafe {
            if GetWindowRect(self.0, &mut r).is_err() {
                return;
            }
        }
        let scale = unsafe { windows::Win32::UI::HiDpi::GetDpiForWindow(self.0) } as f32 / 96.0;
        crate::osd::show(r, scale.max(1.0), text);
    }

    /// Raccourci global de secours, si le hook clavier n'a pas pu être installé.
    /// Nécessite `follow_suggested_dpi_rect` (même sous-classement de fenêtre).
    pub fn register_control_hotkey(self, sc: &crate::shortcuts::Shortcut) -> bool {
        let Some(vk) = sc.vk() else { return false };
        let mut flags = MOD_NOREPEAT;
        if sc.ctrl {
            flags |= MOD_CONTROL;
        }
        if sc.alt {
            flags |= MOD_ALT;
        }
        if sc.shift {
            flags |= MOD_SHIFT;
        }
        unsafe { RegisterHotKey(Some(self.0), HOTKEY_CONTROL_ID, flags, vk).is_ok() }
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
        // Relâcher Alt sur la fenêtre active le menu système invisible, qui avale la touche
        // suivante : Alt sert ici à reprendre la main sur la fenêtre, pas aux menus.
        if msg == WM_SYSCOMMAND && (wparam.0 & 0xFFF0) as u32 == SC_KEYMENU {
            return LRESULT(0);
        }
        if msg == WM_SIZING {
            let result = DefSubclassProc(hwnd, msg, wparam, lparam);
            let aspect = f64::from_bits(RESIZE_ASPECT.load(Ordering::Relaxed));
            if aspect > 0.0 {
                let r = &mut *(lparam.0 as *mut RECT);
                let (mut wr, mut cr) = (RECT::default(), RECT::default());
                let _ = GetWindowRect(hwnd, &mut wr);
                let _ = GetClientRect(hwnd, &mut cr);
                // Épaisseur des bordures Windows (0 sans barre de titre).
                let nc = ((wr.right - wr.left) - (cr.right - cr.left), (wr.bottom - wr.top) - (cr.bottom - cr.top));
                let (l, t, rr, b) = constrain_sizing((r.left, r.top, r.right, r.bottom), wparam.0 as u32, aspect, nc);
                *r = RECT { left: l, top: t, right: rr, bottom: b };
                return LRESULT(1);
            }
            return result;
        }
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

/// Élément du menu du clic droit.
pub enum MenuItem {
    Item { id: u32, label: String, checked: bool },
    Submenu { label: String, items: Vec<MenuItem> },
    Separator,
}

unsafe fn build_menu(items: &[MenuItem]) -> Option<HMENU> {
    unsafe {
        let menu = CreatePopupMenu().ok()?;
        for it in items {
            match it {
                MenuItem::Item { id, label, checked } => {
                    let text: Vec<u16> = label.encode_utf16().chain(Some(0)).collect();
                    let mut flags = MF_STRING;
                    if *checked {
                        flags |= MF_CHECKED;
                    }
                    let _ = AppendMenuW(menu, flags, *id as usize, windows::core::PCWSTR(text.as_ptr()));
                }
                MenuItem::Submenu { label, items } => {
                    let Some(sub) = build_menu(items) else { continue };
                    let text: Vec<u16> = label.encode_utf16().chain(Some(0)).collect();
                    let _ = AppendMenuW(menu, MF_POPUP, sub.0 as usize, windows::core::PCWSTR(text.as_ptr()));
                }
                MenuItem::Separator => {
                    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
                }
            }
        }
        Some(menu)
    }
}

// --- Menu du clic droit -----------------------------------------------------------------
// Ouvert sur son propre thread, avec une petite fenêtre propriétaire invisible : la boucle
// de la fenêtre PiP n'est jamais bloquée, et le menu peut passer au premier plan (sinon il
// se referme aussitôt). Le choix est relevé par `take_menu_choice`.

static MENU_OPEN: AtomicBool = AtomicBool::new(false);
/// Fenêtre propriétaire du menu ouvert (pour le fermer de l'extérieur).
static MENU_HOST: AtomicIsize = AtomicIsize::new(0);

pub fn menu_open() -> bool {
    MENU_OPEN.load(Ordering::Relaxed)
}

/// Ferme le menu ouvert (relâchement d'Alt).
pub fn close_menu() {
    let host = MENU_HOST.load(Ordering::Relaxed);
    if host != 0 {
        unsafe {
            let _ = PostMessageW(Some(HWND(host as *mut _)), WM_CANCELMODE, WPARAM(0), LPARAM(0));
        }
    }
}
static MENU_CHOICE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Ouvre le menu à la position de la souris (sans effet s'il est déjà ouvert).
pub fn open_menu(items: Vec<MenuItem>) {
    if MENU_OPEN.swap(true, Ordering::Relaxed) {
        return;
    }
    std::thread::spawn(move || unsafe {
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::core::w;
        let instance = GetModuleHandleW(None).ok().map(|m| windows::Win32::Foundation::HINSTANCE(m.0));
        static REGISTER: std::sync::Once = std::sync::Once::new();
        REGISTER.call_once(|| {
            let class = WNDCLASSW {
                lpfnWndProc: Some(menu_host_proc),
                hInstance: instance.unwrap_or_default(),
                lpszClassName: w!("PipPbpMenuHost"),
                ..Default::default()
            };
            RegisterClassW(&class);
        });
        let mut p = POINT::default();
        let _ = GetCursorPos(&mut p);
        if let Ok(host) = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            w!("PipPbpMenuHost"),
            w!(""),
            WS_POPUP,
            p.x,
            p.y,
            1,
            1,
            None,
            None,
            instance,
            None,
        ) && let Some(menu) = build_menu(&items)
        {
            MENU_HOST.store(host.0 as isize, Ordering::Relaxed);
            let _ = SetForegroundWindow(host);
            let id = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY, p.x, p.y, None, host, None);
            let _ = PostMessageW(Some(host), WM_NULL, WPARAM(0), LPARAM(0));
            let _ = DestroyMenu(menu);
            MENU_HOST.store(0, Ordering::Relaxed);
            let _ = DestroyWindow(host);
            if id.0 != 0 {
                MENU_CHOICE.store(id.0 as u32, Ordering::Relaxed);
            }
        }
        MENU_OPEN.store(false, Ordering::Relaxed);
    });
}

unsafe extern "system" fn menu_host_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// Choix fait dans le menu depuis le dernier appel.
pub fn take_menu_choice() -> Option<u32> {
    match MENU_CHOICE.swap(0, Ordering::Relaxed) {
        0 => None,
        id => Some(id),
    }
}

/// Ratio largeur / hauteur imposé pendant le redimensionnement à la souris (0 = libre).
static RESIZE_ASPECT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Redimensionnement proportionnel : en tirant un bord ou un coin, la fenêtre garde le
/// format de l'image. `None` = étirement libre.
pub fn set_resize_aspect(aspect: Option<f64>) {
    RESIZE_ASPECT.store(aspect.filter(|a| a.is_finite() && *a > 0.0).unwrap_or(0.0).to_bits(), Ordering::Relaxed);
}

/// Ajuste le rectangle proposé par Windows pendant un redimensionnement (`WM_SIZING`)
/// pour que la zone cliente garde le ratio `aspect`. `edge` = bord tiré (WMSZ_*),
/// `nc` = bordures Windows (largeur, hauteur) à exclure du calcul.
pub fn constrain_sizing(rect: (i32, i32, i32, i32), edge: u32, aspect: f64, nc: (i32, i32)) -> (i32, i32, i32, i32) {
    const MIN_W: i32 = 160;
    let (mut l, mut t, mut r, mut b) = rect;
    let cw = ((r - l) - nc.0).max(MIN_W);
    let ch = ((b - t) - nc.1).max(1);
    let (left, right, top, bottom) = (1, 2, 3, 6);
    match edge {
        // Bord haut ou bas : la hauteur décide, la largeur suit (vers la droite).
        e if e == top || e == bottom => {
            let w = (ch as f64 * aspect).round() as i32;
            r = l + w.max(MIN_W) + nc.0;
        }
        // Bord gauche/droit ou coin : la largeur décide, la hauteur suit.
        _ => {
            let h = (cw as f64 / aspect).round() as i32;
            if edge == left || edge == right {
                b = t + h + nc.1;
            } else if edge == 4 || edge == 5 {
                // Coins du haut : le bas reste fixe.
                t = b - h - nc.1;
            } else {
                b = t + h + nc.1;
            }
            if cw == MIN_W {
                if edge == left || edge == 4 || edge == 7 {
                    l = r - MIN_W - nc.0;
                } else {
                    r = l + MIN_W + nc.0;
                }
            }
        }
    }
    (l, t, r, b)
}
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

// --- Hook clavier permanent ---------------------------------------------------------------
// Un seul hook bas niveau, sur son propre thread (Windows ne l'appelle que si son thread
// attend des messages), avec trois rôles :
//  - partout : reconnaître le raccourci de contrôle (Ctrl+Alt+K par défaut) ;
//  - mode contrôle : envoyer TOUTES les touches à l'autre PC et les bloquer ici ;
//  - curseur sur la fenêtre PiP : déclencher ses raccourcis (les autres touches passent).
// Les touches sont reconnues par leur position (scancode), quelle que soit la disposition.

/// Mode contrôle : destinataire des touches (None = pas de contrôle).
static KEY_SINK: std::sync::Mutex<Option<KeySink>> = std::sync::Mutex::new(None);
static KEY_HOOK_STARTED: AtomicBool = AtomicBool::new(false);
static HOOK_SEEN: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static HOOK_INJECTED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
/// Raccourci de contrôle : scancode + modificateurs (bits 1 Ctrl, 2 Alt, 4 Maj).
static CONTROL_SC: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0x25);
static CONTROL_MODS: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0b011);
/// Raccourcis de la fenêtre PiP : (scancode, modificateurs, numéro d'action, répétable).
static PIP_SHORTCUTS: std::sync::Mutex<Vec<(u16, u8, u8, bool)>> = std::sync::Mutex::new(Vec::new());
static PIP_HWND: AtomicIsize = AtomicIsize::new(0);
static PIP_ON_TOP: AtomicBool = AtomicBool::new(true);
static PIP_FULLSCREEN: AtomicBool = AtomicBool::new(false);
/// Actions déclenchées par le hook, relevées par la fenêtre (`take_pip_actions`).
static PIP_ACTIONS: std::sync::Mutex<Vec<u8>> = std::sync::Mutex::new(Vec::new());
/// Numéro d'action spécial : Échap en plein écran.
pub const ACTION_EXIT_FULLSCREEN: u8 = 255;
/// Touches enfoncées (détection des répétitions) et touches interceptées à l'appui
/// (leur relâchement est intercepté aussi).
static KEYS_DOWN: std::sync::Mutex<Vec<u16>> = std::sync::Mutex::new(Vec::new());
static KEYS_SWALLOWED: std::sync::Mutex<Vec<u16>> = std::sync::Mutex::new(Vec::new());

/// Diagnostic : touches vues par le hook depuis le début du contrôle, dont simulées.
pub fn keyboard_hook_stats() -> (u32, u32) {
    (HOOK_SEEN.load(Ordering::Relaxed), HOOK_INJECTED.load(Ordering::Relaxed))
}

/// Installe le hook clavier permanent pour la fenêtre PiP `window`.
pub fn start_keyboard_hook(window: Hwnd) -> bool {
    PIP_HWND.store(window.0.0 as isize, Ordering::Relaxed);
    if KEY_HOOK_STARTED.load(Ordering::Relaxed) {
        return true;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || unsafe {
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        let module = GetModuleHandleW(None).ok().map(|m| windows::Win32::Foundation::HINSTANCE(m.0));
        let Ok(hook) = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook_proc), module, 0) else {
            let _ = tx.send(false);
            return;
        };
        KEY_HOOK_STARTED.store(true, Ordering::Relaxed);
        let _ = tx.send(true);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {}
        let _ = UnhookWindowsHookEx(hook);
    });
    rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap_or(false)
}

pub fn keyboard_hook_active() -> bool {
    KEY_HOOK_STARTED.load(Ordering::Relaxed)
}

pub fn set_control_shortcut(sc: &crate::shortcuts::Shortcut) {
    CONTROL_SC.store(u32::from(sc.scancode().unwrap_or(0)), Ordering::Relaxed);
    CONTROL_MODS.store(sc.mods(), Ordering::Relaxed);
}

/// Raccourcis actifs quand le curseur est sur la fenêtre PiP.
pub fn set_pip_shortcuts(list: Vec<(u16, u8, u8, bool)>) {
    *PIP_SHORTCUTS.lock().unwrap() = list;
}

/// Fenêtre toujours au premier plan : ses raccourcis marchent dès que le curseur est dans
/// son rectangle. Sinon, seulement si elle est la fenêtre active (pas cachée par une autre).
pub fn set_pip_on_top(on_top: bool) {
    PIP_ON_TOP.store(on_top, Ordering::Relaxed);
}

pub fn set_pip_fullscreen(fullscreen: bool) {
    PIP_FULLSCREEN.store(fullscreen, Ordering::Relaxed);
}

pub fn take_pip_actions() -> Vec<u8> {
    std::mem::take(&mut *PIP_ACTIONS.lock().unwrap())
}

/// Mode contrôle : tout le clavier part vers `sink` (sauf le raccourci de contrôle).
pub fn start_keyboard_capture(sink: KeySink) -> bool {
    *KEY_SINK.lock().unwrap() = Some(sink);
    HOOK_SEEN.store(0, Ordering::Relaxed);
    HOOK_INJECTED.store(0, Ordering::Relaxed);
    keyboard_hook_active()
}

pub fn stop_keyboard_capture() {
    *KEY_SINK.lock().unwrap() = None;
}

/// Le curseur est sur la fenêtre PiP, visible et non recouverte.
fn cursor_on_pip() -> bool {
    unsafe {
        let hwnd = HWND(PIP_HWND.load(Ordering::Relaxed) as *mut _);
        if hwnd.0.is_null() || !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
            return false;
        }
        if !PIP_ON_TOP.load(Ordering::Relaxed) && GetForegroundWindow() != hwnd {
            return false;
        }
        let (mut p, mut r) = (POINT::default(), RECT::default());
        GetCursorPos(&mut p).is_ok()
            && GetWindowRect(hwnd, &mut r).is_ok()
            && p.x >= r.left
            && p.x < r.right
            && p.y >= r.top
            && p.y < r.bottom
    }
}

/// État des modificateurs suivi par le hook lui-même : quand le hook avale Ctrl/Alt/Maj
/// (mode contrôle), Windows ne met pas à jour `GetAsyncKeyState`.
static MODS_DOWN: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// La touche du raccourci a été avalée : l'application au premier plan ne voit qu'un
/// appui sur Alt, et son relâchement activerait sa barre de menu. Une touche neutre
/// (0xE8, non attribuée) l'en empêche, comme le fait AutoHotkey.
fn mask_alt_release() {
    for up in [false, true] {
        let input = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0xE8),
                    wScan: 0,
                    dwFlags: if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) },
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        unsafe { SendInput(&[input], size_of::<INPUT>() as i32) };
    }
}

fn real_modifiers() -> u8 {
    let down = |vk: VIRTUAL_KEY| unsafe { GetAsyncKeyState(vk.0 as i32) as u16 & 0x8000 != 0 };
    u8::from(down(VK_CONTROL)) | (u8::from(down(VK_MENU)) << 1) | (u8::from(down(VK_SHIFT)) << 2)
}

fn track_modifier(vk: u16, down: bool) {
    let bit = if [VK_CONTROL, VK_LCONTROL, VK_RCONTROL].iter().any(|m| m.0 == vk) {
        1
    } else if [VK_MENU, VK_LMENU, VK_RMENU].iter().any(|m| m.0 == vk) {
        2
    } else if [VK_SHIFT, VK_LSHIFT, VK_RSHIFT].iter().any(|m| m.0 == vk) {
        4
    } else {
        return;
    };
    if down {
        MODS_DOWN.fetch_or(bit, Ordering::Relaxed);
    } else {
        MODS_DOWN.fetch_and(!bit, Ordering::Relaxed);
    }
}

unsafe extern "system" fn keyboard_hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if code < 0 {
            return CallNextHookEx(None, code, wparam, lparam);
        }
        let k = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        let down = !k.flags.contains(LLKHF_UP);
        let mut sc = (k.scanCode & 0xFF) as u16;
        if k.flags.contains(LLKHF_EXTENDED) {
            sc |= 0xE000;
        }
        let vk = k.vkCode as u16;
        let is_modifier = [VK_CONTROL, VK_LCONTROL, VK_RCONTROL, VK_MENU, VK_LMENU, VK_RMENU, VK_SHIFT, VK_LSHIFT, VK_RSHIFT]
            .iter()
            .any(|m| m.0 == vk);
        let repeat = {
            let mut keys = KEYS_DOWN.lock().unwrap();
            let was_down = keys.contains(&sc);
            if down && !was_down {
                keys.push(sc);
            } else if !down {
                keys.retain(|&x| x != sc);
            }
            down && was_down
        };
        // Alt maintenu se répète (~30 fois/s) tant qu'il est la dernière touche enfoncée ;
        // chaque répétition fermerait le menu d'Alt + clic droit. Le vrai relâchement
        // d'Alt passe, lui, et ferme le menu comme voulu.
        if repeat && [VK_MENU, VK_LMENU, VK_RMENU].iter().any(|m| m.0 == vk) && MENU_OPEN.load(Ordering::Relaxed) {
            return LRESULT(1);
        }
        // Relâchement d'une touche interceptée à l'appui : intercepté aussi.
        if !down {
            let mut swallowed = KEYS_SWALLOWED.lock().unwrap();
            if let Some(i) = swallowed.iter().position(|&x| x == sc) {
                swallowed.remove(i);
                return LRESULT(1);
            }
        }
        let swallow_press = |sc: u16| {
            KEYS_SWALLOWED.lock().unwrap().push(sc);
            LRESULT(1)
        };
        track_modifier(vk, down);
        let mods = MODS_DOWN.load(Ordering::Relaxed);

        // 1. Raccourci de contrôle, partout.
        if down && !is_modifier && u32::from(sc) == CONTROL_SC.load(Ordering::Relaxed) && mods == CONTROL_MODS.load(Ordering::Relaxed)
        {
            if !repeat {
                HOTKEY_PRESSED.store(true, Ordering::Relaxed);
            }
            return swallow_press(sc);
        }

        // 2. Mode contrôle : tout part vers l'autre PC.
        if let Some(sink) = KEY_SINK.lock().unwrap().as_ref() {
            HOOK_SEEN.fetch_add(1, Ordering::Relaxed);
            // Les claviers « gaming » réinjectent souvent les frappes : elles sont marquées
            // simulées mais viennent bien de l'utilisateur.
            if k.flags.contains(LLKHF_INJECTED) {
                HOOK_INJECTED.fetch_add(1, Ordering::Relaxed);
            }
            sink(sc, down);
            return LRESULT(1);
        }

        // 3. Alt maintenu + curseur sur la fenêtre PiP : ses raccourcis (Alt+A, Alt+1…).
        // Sans Alt, aucune touche n'est captée. Hors mode contrôle, l'état réel des
        // modificateurs (Windows) est fiable : le hook n'en avale aucun.
        // Menu ouvert : les touches servent au menu.
        if down && !is_modifier && !MENU_OPEN.load(Ordering::Relaxed) {
            let real = real_modifiers();
            MODS_DOWN.store(real, Ordering::Relaxed);
            if real & 2 != 0 && cursor_on_pip() {
                let extra = real & !2;
                let hit = PIP_SHORTCUTS.lock().unwrap().iter().find(|(s, m, _, _)| *s == sc && *m == extra).copied();
                if let Some((_, _, action, repeats)) = hit {
                    if !repeat || repeats {
                        PIP_ACTIONS.lock().unwrap().push(action);
                    }
                    mask_alt_release();
                    return swallow_press(sc);
                }
            }
            // Échap quitte le plein écran (sans Alt : on regarde la fenêtre PiP).
            if sc == 0x01 && real == 0 && PIP_FULLSCREEN.load(Ordering::Relaxed) && cursor_on_pip() {
                PIP_ACTIONS.lock().unwrap().push(ACTION_EXIT_FULLSCREEN);
                return swallow_press(sc);
            }
        }
        CallNextHookEx(None, code, wparam, lparam)
    }
}

/// Simule F24 (absente des claviers, sans effet) : sert à vérifier que le hook clavier
/// reçoit bien les touches.
pub fn send_f24() {
    for up in [false, true] {
        let flags = if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) };
        let input = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VK_F24, wScan: 0, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
        };
        unsafe { SendInput(&[input], size_of::<INPUT>() as i32) };
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
    fn sizing_keeps_aspect() {
        let a = 16.0 / 9.0;
        // Bord droit tiré à 1600 de large : la hauteur suit (900).
        assert_eq!(constrain_sizing((0, 0, 1600, 300), 2, a, (0, 0)), (0, 0, 1600, 900));
        // Bord bas tiré à 450 de haut : la largeur suit (800).
        assert_eq!(constrain_sizing((100, 100, 300, 550), 6, a, (0, 0)), (100, 100, 900, 550));
        // Coin haut-gauche : le bas-droit reste fixe, la hauteur suit la largeur.
        assert_eq!(constrain_sizing((200, 0, 1000, 1000), 4, a, (0, 0)), (200, 550, 1000, 1000));
        // Avec une barre de titre (16 x 39 px de bordures), seule la zone cliente compte.
        let (l, t, r, b) = constrain_sizing((0, 0, 1616, 100), 8, a, (16, 39));
        assert_eq!((r - l - 16, b - t - 39), (1600, 900));
    }

    #[test]
    #[ignore]
    fn keyboard_hook_receives_keys() {
        let got = Arc::new(AtomicU32::new(0));
        let g = got.clone();
        assert!(start_keyboard_hook(Hwnd(HWND(std::ptr::null_mut()))));
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
