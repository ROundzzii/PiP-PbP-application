//! Petits appels Win32 dont winit n'a pas l'équivalent : opacité globale de la
//! fenêtre (fenêtre « layered »), position du curseur, zone de travail de l'écran.

use windows::Win32::Foundation::{COLORREF, HWND, POINT, RECT};
use windows::Win32::System::Console::{FreeConsole, GetConsoleProcessList};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_MENU};
use windows::Win32::UI::WindowsAndMessaging::*;
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::Window;

#[derive(Clone, Copy)]
pub struct Hwnd(HWND);

impl Hwnd {
    pub fn of(window: &Window) -> Option<Self> {
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
