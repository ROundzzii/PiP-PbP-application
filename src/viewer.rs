//! Côté affichage : réception TCP -> décodage H.264 -> fenêtre PiP/PbP.

use std::io::{BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::bail;
use openh264::decoder::Decoder;
use openh264::formats::YUVSource;
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{CursorIcon, Fullscreen, ResizeDirection, Window, WindowId, WindowLevel};

use crate::config::ViewConfig;
use crate::discovery;
use crate::protocol::{self, MAGIC, MSG_VIDEO, REQ_KEYFRAME};
use crate::render::{Frame, Renderer};
use crate::win::{self, Hwnd};

/// Fréquence de vérification de la position de la souris (survol).
const HOVER_POLL: Duration = Duration::from_millis(15);
/// Largeur (pixels) des bords utilisables pour redimensionner la fenêtre.
const RESIZE_BORDER: f64 = 8.0;

enum UserEvent {
    NewFrame,
    Status(String),
}

/// Dernière image décodée : le réseau écrase, l'affichage prend.
/// On n'affiche jamais une image en retard, on prend toujours la plus récente.
type Slot = Arc<Mutex<Option<Frame>>>;

pub fn run(cfg: ViewConfig) -> anyhow::Result<()> {
    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let slot: Slot = Arc::default();
    {
        let (slot, proxy, host, port) = (slot.clone(), event_loop.create_proxy(), cfg.host.clone(), cfg.port);
        std::thread::spawn(move || network_loop(&host, port, &slot, &proxy));
    }
    let mut app = App::new(cfg, slot);
    event_loop.run_app(&mut app)?;
    Ok(())
}

/// `host` vide ou "auto" : la source est cherchée automatiquement sur le réseau local.
fn network_loop(host: &str, port: u16, slot: &Slot, proxy: &EventLoopProxy<UserEvent>) {
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
        match receive(&host, port, slot, proxy) {
            Ok(()) => eprintln!("Connexion fermée par la source"),
            Err(e) => eprintln!("Connexion : {e:#}"),
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn receive(host: &str, port: u16, slot: &Slot, proxy: &EventLoopProxy<UserEvent>) -> anyhow::Result<()> {
    let stream = TcpStream::connect((host, port))?;
    stream.set_nodelay(true)?;
    let mut requests = stream.try_clone()?;
    let mut reader = BufReader::with_capacity(1 << 20, stream);

    let mut magic = [0u8; 8];
    reader.read_exact(&mut magic)?;
    if &magic != MAGIC {
        bail!("ce n'est pas un serveur pip-pbp (ou version différente)");
    }
    let _ = proxy.send_event(UserEvent::Status(format!("PiP – {host}")));
    eprintln!("Connecté à {host}:{port}");

    let mut decoder = Decoder::new()?;
    let mut buf = Vec::new();
    let mut stats = (Instant::now(), 0u32, 0.0f64);
    loop {
        if protocol::read_packet(&mut reader, &mut buf)? != MSG_VIDEO {
            continue;
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
                requests.write_all(&[REQ_KEYFRAME])?;
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
}

impl App {
    fn new(cfg: ViewConfig, slot: Slot) -> Self {
        Self {
            cfg,
            slot,
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
        self.hwnd = Hwnd::of(&window);
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

    fn user_event(&mut self, _: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::NewFrame => self.request_redraw(),
            UserEvent::Status(s) => {
                if let Some(w) = &self.window {
                    w.set_title(&s);
                }
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.update_hover();
        event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + HOVER_POLL));
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
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
                r.render(self.cfg.keep_aspect);
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
