//! Côté affichage : réception TCP -> décodage H.264 -> fenêtre PiP/PbP.

use std::collections::HashSet;
use std::io::{BufReader, Read};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::bail;
use openh264::decoder::Decoder;
use openh264::formats::YUVSource;
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey, PhysicalKey};
use winit::platform::scancode::PhysicalKeyExtScancode;
use winit::window::{CursorGrabMode, CursorIcon, Fullscreen, ResizeDirection, Window, WindowId, WindowLevel};

use crate::config::ViewConfig;
use crate::discovery;
use crate::protocol::{
    self, IN_KEY, IN_MOUSE_BUTTON, IN_MOUSE_MOVE, IN_WHEEL, MAGIC, MSG_AUTH, MSG_HELLO, MSG_REPLACED, MSG_VIDEO,
    REQ_AUTH, REQ_KEYFRAME,
};
use crate::render::{BORDER_WIDTH, Frame, Renderer};
use crate::win::{self, Hwnd};

/// Fréquence de vérification de la position de la souris (survol).
const HOVER_POLL: Duration = Duration::from_millis(15);
/// Largeur (pixels) des bords utilisables pour redimensionner la fenêtre.
const RESIZE_BORDER: f64 = 8.0;

enum UserEvent {
    NewFrame,
    Status(String),
    /// Un autre viewer a pris la place de celui-ci sur la source.
    Replaced,
    /// Connexion perdue : le mode contrôle doit être quitté.
    Disconnected,
}

/// Canal de contrôle vers la source (souris / clavier), partagé avec la fenêtre.
struct Link {
    stream: TcpStream,
    /// La source autorise le contrôle à distance.
    allowed: bool,
    /// La source a accepté notre code.
    authed: bool,
}

type SharedLink = Arc<Mutex<Option<Link>>>;

fn send_request(link: &SharedLink, kind: u8, payload: &[u8]) {
    if let Some(l) = link.lock().unwrap().as_mut() {
        let _ = protocol::write_request(&mut l.stream, kind, payload);
    }
}

/// Dernière image décodée : le réseau écrase, l'affichage prend.
/// On n'affiche jamais une image en retard, on prend toujours la plus récente.
type Slot = Arc<Mutex<Option<Frame>>>;

pub fn run(cfg: ViewConfig) -> anyhow::Result<()> {
    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let slot: Slot = Arc::default();
    let link: SharedLink = Arc::default();
    {
        let (slot, link, proxy) = (slot.clone(), link.clone(), event_loop.create_proxy());
        let (host, port, code) = (cfg.host.clone(), cfg.port, cfg.control_code.clone());
        std::thread::spawn(move || network_loop(&host, port, &code, &slot, &link, &proxy));
    }
    let mut app = App::new(cfg, slot, link);
    event_loop.run_app(&mut app)?;
    Ok(())
}

/// `host` vide ou "auto" : la source est cherchée automatiquement sur le réseau local.
fn network_loop(host: &str, port: u16, code: &str, slot: &Slot, link: &SharedLink, proxy: &EventLoopProxy<UserEvent>) {
    let auto = host.is_empty() || host.eq_ignore_ascii_case("auto");
    loop {
        let host = if auto {
            let _ = proxy.send_event(UserEvent::Status("PiP – recherche d'une source sur le réseau…".into()));
            eprintln!("Recherche d'une source pip-pbp sur le réseau local…");
            match discovery::find_source(port, Duration::from_secs(3)) {
                Some((ip, name)) => {
                    eprintln!("Source trouvée : {name} ({ip})");
                    ip.to_string()
                }
                None => continue,
            }
        } else {
            host.to_string()
        };
        let _ = proxy.send_event(UserEvent::Status(format!("Connexion à {host}:{port}…")));
        let result = receive(&host, port, code, slot, link, proxy);
        *link.lock().unwrap() = None;
        let _ = proxy.send_event(UserEvent::Disconnected);
        match result {
            Ok(true) => {
                eprintln!("Une autre fenêtre affiche maintenant cette source : fermeture.");
                let _ = proxy.send_event(UserEvent::Replaced);
                return;
            }
            Ok(false) => eprintln!("Connexion fermée par la source"),
            Err(e) => eprintln!("Connexion : {e:#}"),
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// Renvoie `Ok(true)` si la source signale qu'un autre viewer a pris la place.
fn receive(
    host: &str,
    port: u16,
    code: &str,
    slot: &Slot,
    link: &SharedLink,
    proxy: &EventLoopProxy<UserEvent>,
) -> anyhow::Result<bool> {
    let stream = connect(host, port)?;
    stream.set_nodelay(true)?;
    let requests = stream.try_clone()?;
    let mut reader = BufReader::with_capacity(1 << 20, stream);

    let mut magic = [0u8; 8];
    reader.read_exact(&mut magic)?;
    if &magic != MAGIC {
        bail!("ce n'est pas un serveur pip-pbp (ou version différente)");
    }
    let _ = proxy.send_event(UserEvent::Status(format!("PiP – {host}")));
    eprintln!("Connecté à {host}:{port}");

    let mut buf = Vec::new();
    if protocol::read_packet(&mut reader, &mut buf)? != MSG_HELLO || buf.len() != 1 + protocol::NONCE_LEN {
        bail!("réponse inattendue de la source (versions différentes ?)");
    }
    let allowed = buf[0] != 0;
    *link.lock().unwrap() = Some(Link { stream: requests, allowed, authed: false });
    if !allowed {
        eprintln!("Contrôle à distance : non autorisé par la source (affichage seul)");
    } else if code.trim().is_empty() {
        eprintln!("Contrôle à distance : autorisé par la source, mais aucun code saisi pour cette connexion");
    } else {
        send_request(link, REQ_AUTH, &protocol::auth_proof(code, &buf[1..]));
    }

    let mut decoder = Decoder::new()?;
    let mut stats = (Instant::now(), 0u32, 0.0f64);
    loop {
        match protocol::read_packet(&mut reader, &mut buf)? {
            MSG_VIDEO => {}
            MSG_REPLACED => return Ok(true),
            MSG_AUTH => {
                let ok = buf.first() == Some(&1);
                if let Some(l) = link.lock().unwrap().as_mut() {
                    l.authed = ok;
                }
                if ok {
                    eprintln!("Contrôle à distance : prêt — Ctrl+Alt+K pour prendre / rendre la main");
                } else {
                    eprintln!("Contrôle à distance : code refusé par la source");
                }
                continue;
            }
            _ => continue,
        }
        let t0 = Instant::now();
        match decoder.decode(&buf) {
            Ok(Some(yuv)) => {
                let frame = copy_frame(&yuv);
                *slot.lock().unwrap() = Some(frame);
                let _ = proxy.send_event(UserEvent::NewFrame);
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!("Erreur de décodage ({e}), demande d'une image clé");
                send_request(link, REQ_KEYFRAME, &[]);
            }
        }
        stats.1 += 1;
        stats.2 += t0.elapsed().as_secs_f64() * 1000.0;
        let dt = stats.0.elapsed().as_secs_f64();
        if dt >= 5.0 {
            eprintln!("{:.0} img/s reçues | décodage {:.1} ms/img", stats.1 as f64 / dt, stats.2 / stats.1 as f64);
            stats = (Instant::now(), 0, 0.0);
        }
    }
}

/// Connexion avec un délai court : sans réponse, c'est presque toujours le pare-feu
/// de la source (Windows ne refuse pas la connexion, il l'ignore).
fn connect(host: &str, port: u16) -> anyhow::Result<TcpStream> {
    let addrs = (host, port).to_socket_addrs()?;
    let mut last = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, Duration::from_secs(3)) {
            Ok(s) => return Ok(s),
            Err(e) => last = Some(e),
        }
    }
    match last {
        Some(e) if matches!(e.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock) => bail!(
            "{host}:{port} ne répond pas. Sur le PC source : partage démarré ? \
             Cliquez « Autoriser dans le pare-feu » (onglet Partager cet écran)."
        ),
        Some(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
            bail!("{host} refuse la connexion : le partage n'est pas démarré sur ce PC (ou autre port).")
        }
        Some(e) => Err(e.into()),
        None => bail!("adresse introuvable : {host}"),
    }
}

/// Copie les plans en retirant le padding des lignes (stride) du décodeur.
fn copy_frame(yuv: &impl YUVSource) -> Frame {
    let (w, h) = yuv.dimensions();
    let (sy, su, sv) = yuv.strides();
    let plane = |src: &[u8], stride: usize, w: usize, h: usize| {
        let mut out = Vec::with_capacity(w * h);
        for row in src.chunks(stride).take(h) {
            out.extend_from_slice(&row[..w]);
        }
        out
    };
    Frame {
        width: w as u32,
        height: h as u32,
        y: plane(yuv.y(), sy, w, h),
        u: plane(yuv.u(), su, w / 2, h / 2),
        v: plane(yuv.v(), sv, w / 2, h / 2),
    }
}

struct App {
    cfg: ViewConfig,
    slot: Slot,
    window: Option<Arc<Window>>,
    hwnd: Option<Hwnd>,
    renderer: Option<Renderer>,
    /// La souris survole la fenêtre (et Alt n'est pas enfoncé).
    hovered: bool,
    cursor: PhysicalPosition<f64>,
    modifiers: ModifiersState,
    last_click: Option<Instant>,
    layout_applied: bool,
    link: SharedLink,
    /// Mode contrôle (KVM) : souris et clavier pilotent la source.
    control: bool,
    /// Touches et boutons enfoncés pendant le contrôle, relâchés à la sortie.
    keys_down: Arc<Mutex<HashSet<u16>>>,
    buttons_down: HashSet<u8>,
    /// Le clavier est capturé par le hook bas niveau (touche Windows, Alt+Tab…).
    keyboard_hooked: bool,
    /// Compteurs du mode contrôle (diagnostic) : mouvements, clics, touches envoyés.
    sent: (u32, u32, Arc<std::sync::atomic::AtomicU32>),
}

impl App {
    fn new(cfg: ViewConfig, slot: Slot, link: SharedLink) -> Self {
        Self {
            cfg,
            slot,
            link,
            control: false,
            keys_down: Arc::default(),
            buttons_down: HashSet::new(),
            keyboard_hooked: false,
            sent: (0, 0, Arc::default()),
            window: None,
            hwnd: None,
            renderer: None,
            hovered: false,
            cursor: PhysicalPosition::new(0.0, 0.0),
            modifiers: ModifiersState::empty(),
            last_click: None,
            layout_applied: false,
        }
    }

    fn update_hover(&mut self) {
        if self.control {
            return;
        }
        let (Some(window), Some(hwnd)) = (&self.window, self.hwnd) else { return };
        let hovered = hwnd.cursor_inside() && !win::alt_down();
        if hovered == self.hovered {
            return;
        }
        self.hovered = hovered;
        if self.cfg.click_through_on_hover {
            let _ = window.set_cursor_hittest(!hovered);
        }
        self.apply_opacity();
    }

    fn apply_opacity(&self) {
        if let Some(hwnd) = self.hwnd {
            hwnd.set_opacity(if self.hovered { self.cfg.hover_opacity } else { self.cfg.opacity });
        }
    }

    fn video_aspect(&self) -> f64 {
        match self.renderer.as_ref().and_then(Renderer::video_size) {
            Some((w, h)) if h > 0 => w as f64 / h as f64,
            _ => 16.0 / 9.0,
        }
    }

    /// Place la fenêtre en mode PiP (coin) ou PbP (moitié d'écran).
    fn apply_layout(&mut self, layout: &str) {
        let (Some(window), Some(hwnd)) = (&self.window, self.hwnd) else { return };
        let Some((ax, ay, aw, ah)) = hwnd.work_area() else { return };
        window.set_fullscreen(None);
        let aspect = self.video_aspect();
        let (x, y, w, h) = match layout {
            "pip" => {
                let margin = 16;
                let w = (aw as f32 * self.cfg.pip_scale) as u32;
                let h = (w as f64 / aspect) as u32;
                (ax + aw as i32 - w as i32 - margin, ay + ah as i32 - h as i32 - margin, w, h)
            }
            "pbp-left" => (ax, ay, aw / 2, ah),
            "pbp-right" => (ax + (aw / 2) as i32, ay, aw - aw / 2, ah),
            _ => return,
        };
        let _ = window.request_inner_size(PhysicalSize::new(w.max(64), h.max(36)));
        window.set_outer_position(PhysicalPosition::new(x, y));
        self.cfg.layout = layout.to_string();
        self.apply_opacity();
    }

    fn resize_direction(&self) -> Option<ResizeDirection> {
        let window = self.window.as_ref()?;
        if window.fullscreen().is_some() {
            return None;
        }
        let size = window.inner_size();
        let (x, y) = (self.cursor.x, self.cursor.y);
        let (w, h) = (size.width as f64, size.height as f64);
        let (l, r, t, b) = (x < RESIZE_BORDER, x > w - RESIZE_BORDER, y < RESIZE_BORDER, y > h - RESIZE_BORDER);
        Some(match (l, r, t, b) {
            (true, _, true, _) => ResizeDirection::NorthWest,
            (_, true, true, _) => ResizeDirection::NorthEast,
            (true, _, _, true) => ResizeDirection::SouthWest,
            (_, true, _, true) => ResizeDirection::SouthEast,
            (true, ..) => ResizeDirection::West,
            (_, true, ..) => ResizeDirection::East,
            (_, _, true, _) => ResizeDirection::North,
            (_, _, _, true) => ResizeDirection::South,
            _ => return None,
        })
    }

    /// Molette : agrandit / réduit la fenêtre en gardant le ratio de la vidéo.
    fn zoom(&mut self, steps: f64) {
        let Some(window) = &self.window else { return };
        let size = window.inner_size();
        let factor = 1.1f64.powf(steps);
        let w = (size.width as f64 * factor).clamp(160.0, 7680.0);
        let h = w / self.video_aspect();
        let pos = window.outer_position().unwrap_or_default();
        // Zoom centré sur la fenêtre.
        let dx = (w - size.width as f64) / 2.0;
        let dy = (h - size.height as f64) / 2.0;
        let _ = window.request_inner_size(PhysicalSize::new(w as u32, h as u32));
        window.set_outer_position(PhysicalPosition::new(pos.x - dx as i32, pos.y - dy as i32));
    }

    fn toggle_fullscreen(&self) {
        if let Some(window) = &self.window {
            let fs = window.fullscreen().is_some();
            window.set_fullscreen((!fs).then_some(Fullscreen::Borderless(None)));
        }
        self.apply_opacity();
    }

    fn on_key(&mut self, event_loop: &ActiveEventLoop, key: &Key) {
        let step = 0.05;
        match key {
            Key::Named(NamedKey::F11) => self.toggle_fullscreen(),
            Key::Named(NamedKey::Escape) => {
                if let Some(w) = &self.window {
                    w.set_fullscreen(None);
                }
                self.apply_opacity();
            }
            Key::Named(NamedKey::ArrowUp) => {
                self.cfg.hover_opacity = (self.cfg.hover_opacity + step).min(1.0);
                eprintln!("Opacité au survol : {:.0} %", self.cfg.hover_opacity * 100.0);
            }
            Key::Named(NamedKey::ArrowDown) => {
                self.cfg.hover_opacity = (self.cfg.hover_opacity - step).max(0.0);
                eprintln!("Opacité au survol : {:.0} %", self.cfg.hover_opacity * 100.0);
            }
            Key::Named(NamedKey::PageUp) => {
                self.cfg.opacity = (self.cfg.opacity + step).min(1.0);
                self.apply_opacity();
            }
            Key::Named(NamedKey::PageDown) => {
                self.cfg.opacity = (self.cfg.opacity - step).max(0.1);
                self.apply_opacity();
            }
            Key::Character(c) => match c.to_lowercase().as_str() {
                "q" if self.modifiers.control_key() => event_loop.exit(),
                "f" => self.toggle_fullscreen(),
                "1" => self.apply_layout("pip"),
                "2" => self.apply_layout("pbp-left"),
                "3" => self.apply_layout("pbp-right"),
                "a" => {
                    self.cfg.keep_aspect = !self.cfg.keep_aspect;
                    self.request_redraw();
                }
                "c" => {
                    self.cfg.click_through_on_hover = !self.cfg.click_through_on_hover;
                    eprintln!("Clic à travers au survol : {}", self.cfg.click_through_on_hover);
                }
                "t" => {
                    self.cfg.always_on_top = !self.cfg.always_on_top;
                    if let Some(w) = &self.window {
                        w.set_window_level(level(self.cfg.always_on_top));
                    }
                    self.apply_opacity();
                }
                _ => {}
            },
            _ => {}
        }
    }

    /// Ctrl+Alt+K : prend ou rend la main sur la source.
    fn toggle_control(&mut self) {
        if self.control {
            self.exit_control();
            return;
        }
        let state = self.link.lock().unwrap().as_ref().map(|l| (l.allowed, l.authed));
        let refusal = match state {
            None => Some("pas connecté à une source"),
            Some((false, _)) => Some("la source n'autorise pas le contrôle (case à cocher dans « Partager cet écran »)"),
            Some((true, false)) if self.cfg.control_code.trim().is_empty() => {
                Some("aucun code saisi pour cette connexion (onglet Connexions)")
            }
            Some((true, false)) => Some("code refusé par la source"),
            Some((true, true)) => None,
        };
        if let Some(reason) = refusal {
            eprintln!("Contrôle à distance impossible : {reason}");
            return;
        }
        let Some(window) = self.window.clone() else { return };
        self.control = true;
        self.hovered = false;
        let _ = window.set_cursor_hittest(true);
        if let Some(h) = self.hwnd {
            h.set_opacity(1.0);
        }
        window.focus_window();
        // La souris reste dans la fenêtre et le curseur local disparaît : on ne voit plus
        // que celui de l'autre PC, dans l'image.
        let _ = window.set_cursor_grab(CursorGrabMode::Confined);
        window.set_cursor_visible(false);
        // Tout le clavier part vers l'autre PC, y compris la touche Windows et Alt+Tab.
        let (link, keys_down, keys_sent) = (self.link.clone(), self.keys_down.clone(), self.sent.2.clone());
        self.keyboard_hooked = win::start_keyboard_capture(Box::new(move |sc, down| {
            if down {
                keys_down.lock().unwrap().insert(sc);
            } else {
                keys_down.lock().unwrap().remove(&sc);
            }
            keys_sent.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let mut p = sc.to_le_bytes().to_vec();
            p.push(u8::from(down));
            send_request(&link, IN_KEY, &p);
        }));
        if !self.keyboard_hooked {
            eprintln!("Capture complète du clavier indisponible : la touche Windows et Alt+Tab resteront sur ce PC");
        }
        self.sent.0 = 0;
        self.sent.1 = 0;
        self.sent.2.store(0, std::sync::atomic::Ordering::Relaxed);
        eprintln!("Contrôle à distance : ACTIF (Ctrl+Alt+K pour rendre la main)");
        self.request_redraw();
    }

    fn exit_control(&mut self) {
        if !self.control {
            return;
        }
        self.control = false;
        win::stop_keyboard_capture();
        self.keyboard_hooked = false;
        // Jamais de touche ou de bouton coincé sur l'autre PC.
        let keys: Vec<u16> = self.keys_down.lock().unwrap().drain().collect();
        for sc in keys {
            let mut p = sc.to_le_bytes().to_vec();
            p.push(0);
            send_request(&self.link, IN_KEY, &p);
        }
        for b in self.buttons_down.drain() {
            send_request(&self.link, IN_MOUSE_BUTTON, &[b, 0]);
        }
        if let Some(window) = &self.window {
            let _ = window.set_cursor_grab(CursorGrabMode::None);
            window.set_cursor_visible(true);
        }
        self.hovered = false;
        self.apply_opacity();
        eprintln!(
            "Contrôle à distance : rendu ({} mouvements de souris, {} clics/molette, {} touches envoyés)",
            self.sent.0,
            self.sent.1,
            self.sent.2.load(std::sync::atomic::Ordering::Relaxed)
        );
        self.request_redraw();
    }

    /// Mode contrôle : transmet l'évènement à la source. Renvoie `true` s'il est consommé.
    fn forward_input(&mut self, event: &WindowEvent) -> bool {
        match event {
            WindowEvent::CursorMoved { position, .. } => {
                let rect = self.renderer.as_ref().and_then(|r| r.video_rect(self.cfg.keep_aspect, BORDER_WIDTH));
                let Some((x, y, w, h)) = rect else { return true };
                let fx = ((position.x as f32 - x) / w).clamp(0.0, 1.0);
                let fy = ((position.y as f32 - y) / h).clamp(0.0, 1.0);
                let mut p = ((fx * 65535.0) as u16).to_le_bytes().to_vec();
                p.extend_from_slice(&((fy * 65535.0) as u16).to_le_bytes());
                send_request(&self.link, IN_MOUSE_MOVE, &p);
                self.sent.0 += 1;
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let b = match button {
                    MouseButton::Left => 0,
                    MouseButton::Right => 1,
                    MouseButton::Middle => 2,
                    MouseButton::Back => 3,
                    MouseButton::Forward => 4,
                    MouseButton::Other(_) => return true,
                };
                let down = *state == ElementState::Pressed;
                if down {
                    self.buttons_down.insert(b);
                } else {
                    self.buttons_down.remove(&b);
                }
                send_request(&self.link, IN_MOUSE_BUTTON, &[b, u8::from(down)]);
                self.sent.1 += 1;
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x * 120.0, y * 120.0),
                    MouseScrollDelta::PixelDelta(p) => (p.x as f32, p.y as f32),
                };
                let mut p = (dy as i16).to_le_bytes().to_vec();
                p.extend_from_slice(&(dx as i16).to_le_bytes());
                send_request(&self.link, IN_WHEEL, &p);
                self.sent.1 += 1;
            }
            // Avec le hook, le clavier est déjà envoyé (et bloqué) avant d'arriver ici.
            WindowEvent::KeyboardInput { .. } if self.keyboard_hooked => {}
            WindowEvent::KeyboardInput { event, .. } => {
                if let PhysicalKey::Code(_) = event.physical_key
                    && let Some(sc) = event.physical_key.to_scancode()
                {
                    let sc = sc as u16;
                    let down = event.state == ElementState::Pressed;
                    if down {
                        self.keys_down.lock().unwrap().insert(sc);
                    } else {
                        self.keys_down.lock().unwrap().remove(&sc);
                    }
                    self.sent.2.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let mut p = sc.to_le_bytes().to_vec();
                    p.push(u8::from(down));
                    send_request(&self.link, IN_KEY, &p);
                }
            }
            _ => return false,
        }
        true
    }

    fn request_redraw(&self) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }
}

fn level(on_top: bool) -> WindowLevel {
    if on_top { WindowLevel::AlwaysOnTop } else { WindowLevel::Normal }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("PiP – connexion…")
            .with_inner_size(PhysicalSize::new(self.cfg.width, self.cfg.height))
            .with_decorations(self.cfg.decorations)
            .with_resizable(true)
            .with_window_level(level(self.cfg.always_on_top));
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                eprintln!("Impossible de créer la fenêtre : {e}");
                event_loop.exit();
                return;
            }
        };
        self.hwnd = Hwnd::of(&*window);
        if let Some(h) = self.hwnd {
            h.follow_suggested_dpi_rect();
            if !h.register_control_hotkey() {
                eprintln!("Raccourci Ctrl+Alt+K indisponible (déjà utilisé par une autre application)");
            }
        }
        match Renderer::new(window.clone()) {
            Ok(r) => self.renderer = Some(r),
            Err(e) => {
                eprintln!("Erreur GPU : {e:#}");
                event_loop.exit();
                return;
            }
        }
        self.window = Some(window);
        self.apply_opacity();
        let layout = self.cfg.layout.clone();
        self.apply_layout(&layout);
        self.request_redraw();
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::NewFrame => self.request_redraw(),
            UserEvent::Replaced => event_loop.exit(),
            UserEvent::Disconnected => self.exit_control(),
            UserEvent::Status(s) => {
                if let Some(w) = &self.window {
                    w.set_title(&s);
                }
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if win::take_control_hotkey() {
            self.toggle_control();
        }
        self.update_hover();
        event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + HOVER_POLL));
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        if self.control {
            if let WindowEvent::Focused(false) = event {
                // Alt+Tab ou clic ailleurs : on rend la main, par sécurité.
                self.exit_control();
            } else if self.forward_input(&event) {
                return;
            }
        }
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(r) = &mut self.renderer {
                    r.resize(size.width, size.height);
                }
                self.request_redraw();
            }
            WindowEvent::RedrawRequested => {
                let Some(r) = &mut self.renderer else { return };
                if let Some(frame) = self.slot.lock().unwrap().take() {
                    r.upload(&frame);
                }
                let border = self.control.then_some(wgpu::Color { r: 1.0, g: 0.45, b: 0.0, a: 1.0 });
                r.render(self.cfg.keep_aspect, border);
                // Le ratio de la vidéo est maintenant connu : on recale la disposition PiP.
                if !self.layout_applied && r.video_size().is_some() {
                    self.layout_applied = true;
                    let layout = self.cfg.layout.clone();
                    self.apply_layout(&layout);
                }
            }
            WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = position;
                if let Some(w) = &self.window {
                    let icon = match self.resize_direction() {
                        Some(ResizeDirection::East | ResizeDirection::West) => CursorIcon::EwResize,
                        Some(ResizeDirection::North | ResizeDirection::South) => CursorIcon::NsResize,
                        Some(ResizeDirection::NorthWest | ResizeDirection::SouthEast) => CursorIcon::NwseResize,
                        Some(ResizeDirection::NorthEast | ResizeDirection::SouthWest) => CursorIcon::NeswResize,
                        None => CursorIcon::Move,
                    };
                    w.set_cursor(icon);
                }
            }
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Left, .. } => {
                let now = Instant::now();
                if self.last_click.is_some_and(|t| now - t < Duration::from_millis(350)) {
                    self.last_click = None;
                    self.toggle_fullscreen();
                    return;
                }
                self.last_click = Some(now);
                let Some(w) = &self.window else { return };
                let _ = match self.resize_direction() {
                    Some(dir) => w.drag_resize_window(dir),
                    None => w.drag_window(),
                };
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let steps = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y as f64,
                    MouseScrollDelta::PixelDelta(p) => p.y / 120.0,
                };
                self.zoom(steps);
            }
            WindowEvent::KeyboardInput { event: KeyEvent { logical_key, state: ElementState::Pressed, .. }, .. } => {
                self.on_key(event_loop, &logical_key);
            }
            _ => {}
        }
    }
}
