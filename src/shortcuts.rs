//! Raccourcis clavier de la fenêtre PiP, modifiables depuis l'interface.
//!
//! Les touches sont désignées par leur **position** sur le clavier (comme un clavier
//! QWERTY : « A », « 1 », « F11 », « ArrowUp »…), indépendamment de la disposition
//! (AZERTY, etc.) : la touche « 1 » de la rangée du haut fonctionne même si elle
//! produit « & ».

use std::collections::BTreeMap;
use std::fmt;

/// Actions déclenchables au clavier ou depuis le menu du clic droit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    ToggleControl,
    Fullscreen,
    LayoutPip,
    LayoutPbpLeft,
    LayoutPbpRight,
    ToggleAspect,
    ToggleClickThrough,
    ToggleOnTop,
    ToggleLatency,
    HoverOpacityUp,
    HoverOpacityDown,
    OpacityUp,
    OpacityDown,
    Close,
}

impl Action {
    pub const ALL: [Action; 14] = [
        Action::ToggleControl,
        Action::Fullscreen,
        Action::LayoutPip,
        Action::LayoutPbpLeft,
        Action::LayoutPbpRight,
        Action::ToggleAspect,
        Action::ToggleClickThrough,
        Action::ToggleOnTop,
        Action::ToggleLatency,
        Action::HoverOpacityUp,
        Action::HoverOpacityDown,
        Action::OpacityUp,
        Action::OpacityDown,
        Action::Close,
    ];

    /// Nom dans le fichier de configuration (`[view.shortcuts]`).
    pub fn id(self) -> &'static str {
        match self {
            Action::ToggleControl => "toggle_control",
            Action::Fullscreen => "fullscreen",
            Action::LayoutPip => "layout_pip",
            Action::LayoutPbpLeft => "layout_pbp_left",
            Action::LayoutPbpRight => "layout_pbp_right",
            Action::ToggleAspect => "toggle_free_stretch",
            Action::ToggleClickThrough => "toggle_click_through",
            Action::ToggleOnTop => "toggle_always_on_top",
            Action::ToggleLatency => "toggle_latency",
            Action::HoverOpacityUp => "hover_opacity_up",
            Action::HoverOpacityDown => "hover_opacity_down",
            Action::OpacityUp => "opacity_up",
            Action::OpacityDown => "opacity_down",
            Action::Close => "close",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Action::ToggleControl => "Contrôle de l'autre PC (prendre / rendre la main)",
            Action::Fullscreen => "Plein écran / fenêtré",
            Action::LayoutPip => "Disposition PiP (coin bas-droit)",
            Action::LayoutPbpLeft => "Disposition PbP (moitié gauche)",
            Action::LayoutPbpRight => "Disposition PbP (moitié droite)",
            Action::ToggleAspect => "Étirement libre activé / désactivé",
            Action::ToggleClickThrough => "Clic à travers au survol",
            Action::ToggleOnTop => "Toujours au premier plan",
            Action::ToggleLatency => "Afficher la latence",
            Action::HoverOpacityUp => "Opacité au survol +5 %",
            Action::HoverOpacityDown => "Opacité au survol −5 %",
            Action::OpacityUp => "Opacité normale +5 %",
            Action::OpacityDown => "Opacité normale −5 %",
            Action::Close => "Fermer la fenêtre PiP",
        }
    }

    pub fn default_shortcut(self) -> &'static str {
        match self {
            Action::ToggleControl => "Ctrl+Alt+K",
            Action::Fullscreen => "F11",
            Action::LayoutPip => "1",
            Action::LayoutPbpLeft => "2",
            Action::LayoutPbpRight => "3",
            Action::ToggleAspect => "A",
            Action::ToggleClickThrough => "C",
            Action::ToggleOnTop => "T",
            Action::ToggleLatency => "L",
            Action::HoverOpacityUp => "ArrowUp",
            Action::HoverOpacityDown => "ArrowDown",
            Action::OpacityUp => "PageUp",
            Action::OpacityDown => "PageDown",
            Action::Close => "Q",
        }
    }

    /// Le raccourci agit sur tout le PC (pas seulement quand la fenêtre a le focus) :
    /// il doit contenir Ctrl ou Alt pour ne pas « voler » une touche normale.
    pub fn is_global(self) -> bool {
        self == Action::ToggleControl
    }

    /// Les répétitions de touche (touche maintenue) déclenchent l'action plusieurs fois.
    pub fn repeats(self) -> bool {
        matches!(self, Action::HoverOpacityUp | Action::HoverOpacityDown | Action::OpacityUp | Action::OpacityDown)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Default)]
pub struct Shortcut {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    /// Touche principale, nom canonique (« K », « 1 », « F11 », « ArrowUp »…).
    pub key: String,
}

impl Shortcut {
    /// Lit « Ctrl+Alt+K », « Maj+F5 », « ArrowUp »… (insensible à la casse des modificateurs).
    pub fn parse(s: &str) -> Option<Self> {
        let mut sc = Shortcut::default();
        for part in s.split('+').map(str::trim).filter(|p| !p.is_empty()) {
            match part.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => sc.ctrl = true,
                "alt" => sc.alt = true,
                "shift" | "maj" => sc.shift = true,
                _ => {
                    if !sc.key.is_empty() {
                        return None;
                    }
                    sc.key = canonical(part)?;
                }
            }
        }
        (!sc.key.is_empty()).then_some(sc)
    }

    /// Texte lisible pour l'interface et les menus (« Ctrl+Alt+K », « ↑ », « Page↑ »).
    pub fn display(&self) -> String {
        let mut s = String::new();
        if self.ctrl {
            s.push_str("Ctrl+");
        }
        if self.alt {
            s.push_str("Alt+");
        }
        if self.shift {
            s.push_str("Maj+");
        }
        s.push_str(match self.key.as_str() {
            "ArrowUp" => "↑",
            "ArrowDown" => "↓",
            "ArrowLeft" => "←",
            "ArrowRight" => "→",
            "PageUp" => "Page↑",
            "PageDown" => "Page↓",
            "Escape" => "Échap",
            "Space" => "Espace",
            "Enter" => "Entrée",
            "Delete" => "Suppr",
            "Backspace" => "Retour arrière",
            "Insert" => "Inser",
            "Home" => "Début",
            "End" => "Fin",
            "Tab" => "Tab",
            other => other,
        });
        s
    }

    /// Code de touche virtuelle Windows (raccourci global et hook clavier).
    pub fn vk(&self) -> Option<u32> {
        let k = self.key.as_str();
        if k.len() == 1 {
            let c = k.as_bytes()[0];
            if c.is_ascii_uppercase() || c.is_ascii_digit() {
                return Some(c as u32);
            }
        }
        if let Some(n) = k.strip_prefix('F').and_then(|n| n.parse::<u32>().ok())
            && (1..=24).contains(&n)
        {
            return Some(0x70 + n - 1);
        }
        if let Some(n) = k.strip_prefix("Numpad").and_then(|n| n.parse::<u32>().ok())
            && n <= 9
        {
            return Some(0x60 + n);
        }
        Some(match k {
            "ArrowLeft" => 0x25,
            "ArrowUp" => 0x26,
            "ArrowRight" => 0x27,
            "ArrowDown" => 0x28,
            "PageUp" => 0x21,
            "PageDown" => 0x22,
            "End" => 0x23,
            "Home" => 0x24,
            "Insert" => 0x2D,
            "Delete" => 0x2E,
            "Space" => 0x20,
            "Enter" => 0x0D,
            "Tab" => 0x09,
            "Escape" => 0x1B,
            "Backspace" => 0x08,
            _ => return None,
        })
    }

    /// Raccourci de la fenêtre PiP : Alt est implicite (il faut tenir Alt, curseur sur la
    /// fenêtre). On le retire de la combinaison enregistrée.
    pub fn without_alt(&self) -> Self {
        Shortcut { alt: false, ..self.clone() }
    }

    /// Affichage d'un raccourci de la fenêtre PiP (« Alt+A »).
    pub fn pip_display(&self) -> String {
        format!("Alt+{}", self.without_alt().display())
    }

    /// Affichage selon le type d'action (global ou fenêtre PiP).
    pub fn display_for(&self, action: Action) -> String {
        if action.is_global() { self.display() } else { self.pip_display() }
    }

    /// Modificateurs en bits : 1 = Ctrl, 2 = Alt, 4 = Maj.
    pub fn mods(&self) -> u8 {
        u8::from(self.ctrl) | (u8::from(self.alt) << 1) | (u8::from(self.shift) << 2)
    }

    /// Scancode Windows de la touche (position physique, 0xE0xx = touche étendue).
    /// Sert au hook clavier, qui reconnaît les touches par leur position.
    pub fn scancode(&self) -> Option<u16> {
        const LETTERS: &[(&str, u16)] = &[
            ("Q", 0x10), ("W", 0x11), ("E", 0x12), ("R", 0x13), ("T", 0x14), ("Y", 0x15), ("U", 0x16), ("I", 0x17),
            ("O", 0x18), ("P", 0x19), ("A", 0x1E), ("S", 0x1F), ("D", 0x20), ("F", 0x21), ("G", 0x22), ("H", 0x23),
            ("J", 0x24), ("K", 0x25), ("L", 0x26), ("Z", 0x2C), ("X", 0x2D), ("C", 0x2E), ("V", 0x2F), ("B", 0x30),
            ("N", 0x31), ("M", 0x32),
        ];
        const OTHERS: &[(&str, u16)] = &[
            ("1", 0x02), ("2", 0x03), ("3", 0x04), ("4", 0x05), ("5", 0x06), ("6", 0x07), ("7", 0x08), ("8", 0x09),
            ("9", 0x0A), ("0", 0x0B), ("Escape", 0x01), ("Backspace", 0x0E), ("Tab", 0x0F), ("Enter", 0x1C),
            ("Space", 0x39), ("F1", 0x3B), ("F2", 0x3C), ("F3", 0x3D), ("F4", 0x3E), ("F5", 0x3F), ("F6", 0x40),
            ("F7", 0x41), ("F8", 0x42), ("F9", 0x43), ("F10", 0x44), ("F11", 0x57), ("F12", 0x58), ("F13", 0x64),
            ("F14", 0x65), ("F15", 0x66), ("F16", 0x67), ("F17", 0x68), ("F18", 0x69), ("F19", 0x6A), ("F20", 0x6B),
            ("F21", 0x6C), ("F22", 0x6D), ("F23", 0x6E), ("F24", 0x76), ("Numpad0", 0x52), ("Numpad1", 0x4F),
            ("Numpad2", 0x50), ("Numpad3", 0x51), ("Numpad4", 0x4B), ("Numpad5", 0x4C), ("Numpad6", 0x4D),
            ("Numpad7", 0x47), ("Numpad8", 0x48), ("Numpad9", 0x49), ("ArrowUp", 0xE048), ("ArrowDown", 0xE050),
            ("ArrowLeft", 0xE04B), ("ArrowRight", 0xE04D), ("PageUp", 0xE049), ("PageDown", 0xE051),
            ("Home", 0xE047), ("End", 0xE04F), ("Insert", 0xE052), ("Delete", 0xE053),
        ];
        LETTERS.iter().chain(OTHERS).find(|(k, _)| *k == self.key).map(|(_, sc)| *sc)
    }

    /// Touche de la fenêtre PiP (position physique winit) + modificateurs.
    pub fn from_winit(code: winit::keyboard::KeyCode, mods: winit::keyboard::ModifiersState) -> Option<Self> {
        let name = format!("{code:?}");
        let key = if let Some(letter) = name.strip_prefix("Key") {
            letter.to_string()
        } else if let Some(digit) = name.strip_prefix("Digit") {
            digit.to_string()
        } else {
            name
        };
        Some(Shortcut { ctrl: mods.control_key(), alt: mods.alt_key(), shift: mods.shift_key(), key: canonical(&key)? })
    }

    /// Touche capturée dans l'interface (egui) + modificateurs.
    pub fn from_egui(key: eframe::egui::Key, mods: eframe::egui::Modifiers) -> Option<Self> {
        let name = format!("{key:?}");
        let name = name.strip_prefix("Num").filter(|d| d.len() == 1).map(str::to_string).unwrap_or(name);
        Some(Shortcut { ctrl: mods.ctrl, alt: mods.alt, shift: mods.shift, key: canonical(&name)? })
    }
}

impl fmt::Display for Shortcut {
    /// Forme enregistrée dans le fichier de configuration (« Ctrl+Alt+K », « ArrowUp »).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.ctrl {
            f.write_str("Ctrl+")?;
        }
        if self.alt {
            f.write_str("Alt+")?;
        }
        if self.shift {
            f.write_str("Shift+")?;
        }
        f.write_str(&self.key)
    }
}

/// Nom canonique d'une touche, ou `None` si elle n'est pas utilisable comme raccourci.
fn canonical(name: &str) -> Option<String> {
    let n = name.trim();
    if n.len() == 1 && n.as_bytes()[0].is_ascii_alphanumeric() {
        return Some(n.to_ascii_uppercase());
    }
    const NAMED: [&str; 15] = [
        "ArrowUp", "ArrowDown", "ArrowLeft", "ArrowRight", "PageUp", "PageDown", "Home", "End", "Insert", "Delete",
        "Space", "Enter", "Tab", "Escape", "Backspace",
    ];
    if let Some(found) = NAMED.iter().find(|k| k.eq_ignore_ascii_case(n)) {
        return Some(found.to_string());
    }
    let upper = n.to_ascii_uppercase();
    if let Some(num) = upper.strip_prefix('F').and_then(|x| x.parse::<u32>().ok())
        && (1..=24).contains(&num)
    {
        return Some(format!("F{num}"));
    }
    if let Some(num) = n.strip_prefix("Numpad").and_then(|x| x.parse::<u32>().ok())
        && num <= 9
    {
        return Some(format!("Numpad{num}"));
    }
    None
}

/// Raccourcis effectifs : ceux du fichier de configuration, sinon ceux par défaut.
#[derive(Clone, Debug)]
pub struct Shortcuts(Vec<(Action, Shortcut)>);

impl Shortcuts {
    pub fn from_config(map: &BTreeMap<String, String>) -> Self {
        Self(
            Action::ALL
                .iter()
                .map(|&a| {
                    let configured = map.get(a.id()).and_then(|s| Shortcut::parse(s));
                    let sc = configured.unwrap_or_else(|| Shortcut::parse(a.default_shortcut()).expect("raccourci par défaut valide"));
                    (a, sc)
                })
                .collect(),
        )
    }

    pub fn get(&self, action: Action) -> &Shortcut {
        &self.0.iter().find(|(a, _)| *a == action).expect("toutes les actions ont un raccourci").1
    }

    #[cfg(test)]
    pub fn action_for(&self, sc: &Shortcut) -> Option<Action> {
        self.0.iter().find(|(_, s)| s == sc).map(|(a, _)| *a)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_display_vk() {
        let sc = Shortcut::parse("ctrl+alt+k").unwrap();
        assert_eq!(sc.to_string(), "Ctrl+Alt+K");
        assert_eq!(sc.vk(), Some(b'K' as u32));
        assert_eq!(Shortcut::parse("Maj+F5").unwrap().to_string(), "Shift+F5");
        assert_eq!(Shortcut::parse("F5").unwrap().vk(), Some(0x74));
        assert_eq!(Shortcut::parse("arrowup").unwrap().display(), "↑");
        assert_eq!(Shortcut::parse("1").unwrap().vk(), Some(b'1' as u32));
        assert!(Shortcut::parse("Ctrl+").is_none());
        assert!(Shortcut::parse("A+B").is_none());
        assert!(Shortcut::parse("Ctrl+§").is_none());
    }

    #[test]
    fn winit_and_egui_agree() {
        use winit::keyboard::{KeyCode, ModifiersState};
        let w = Shortcut::from_winit(KeyCode::Digit1, ModifiersState::empty()).unwrap();
        let e = Shortcut::from_egui(eframe::egui::Key::Num1, eframe::egui::Modifiers::NONE).unwrap();
        assert_eq!(w, e);
        let w = Shortcut::from_winit(KeyCode::KeyK, ModifiersState::CONTROL | ModifiersState::ALT).unwrap();
        let e = Shortcut::from_egui(eframe::egui::Key::K, eframe::egui::Modifiers { ctrl: true, alt: true, ..Default::default() })
            .unwrap();
        assert_eq!(w, e);
        assert_eq!(
            Shortcut::from_winit(KeyCode::ArrowUp, ModifiersState::empty()),
            Shortcut::from_egui(eframe::egui::Key::ArrowUp, eframe::egui::Modifiers::NONE)
        );
    }

    #[test]
    fn config_overrides_defaults() {
        let mut map = BTreeMap::new();
        map.insert("toggle_free_stretch".to_string(), "Shift+S".to_string());
        let s = Shortcuts::from_config(&map);
        assert_eq!(s.action_for(&Shortcut::parse("Shift+S").unwrap()), Some(Action::ToggleAspect));
        assert_eq!(s.action_for(&Shortcut::parse("A").unwrap()), None);
        assert_eq!(s.get(Action::Close).to_string(), "Q");
        assert_eq!(s.get(Action::Close).display_for(Action::Close), "Alt+Q");
        assert_eq!(Shortcut::parse("Alt+A").unwrap().pip_display(), "Alt+A");
    }
}
