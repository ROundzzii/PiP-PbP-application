//! Interface graphique (egui) : connexions enregistrées, recherche sur le réseau,
//! partage de l'écran et réglages. Lancée quand `pip-pbp` est démarré sans commande.
//!
//! Le viewer et la source tournent dans des processus séparés (le même exécutable
//! avec `view` / `send`) : une fenêtre PiP plantée ne fait pas tomber l'interface,
//! et chaque session a son propre journal.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::net::IpAddr;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText};

use crate::capture;
use crate::config::{self, Connection, FileConfig};
use crate::discovery;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const MAX_LOG_LINES: usize = 300;

pub fn run() -> anyhow::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("PiP-PbP")
            .with_inner_size([760.0, 620.0])
            .with_min_inner_size([560.0, 420.0]),
        ..Default::default()
    };
    eframe::run_native("PiP-PbP", options, Box::new(|_cc| Ok(Box::new(Gui::new()))))
        .map_err(|e| anyhow::anyhow!("interface : {e}"))
}

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Connections,
    Share,
    Display,
}

#[derive(PartialEq, Clone, Copy)]
enum Kind {
    View,
    Send,
}

/// Un processus `pip-pbp view` ou `pip-pbp send` lancé depuis l'interface.
struct Session {
    kind: Kind,
    label: String,
    /// `None` si le processus n'a pas pu être lancé.
    child: Option<Child>,
    log: Arc<Mutex<VecDeque<String>>>,
    exit: Option<String>,
}

impl Session {
    fn is_running(&self) -> bool {
        self.exit.is_none()
    }

    fn kill(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
        }
    }
}

struct Gui {
    cfg: FileConfig,
    path: PathBuf,
    saved: FileConfig,
    changed_at: Option<Instant>,
    save_error: Option<String>,

    tab: Tab,
    quick_host: String,
    new_conn: Connection,
    editing: Option<usize>,

    search: Option<Receiver<Vec<(IpAddr, String)>>>,
    found: Vec<(IpAddr, String)>,
    searched: bool,

    monitors: Vec<String>,
    local_ips: Vec<(String, String)>,
    sessions: Vec<Session>,
}

impl Gui {
    fn new() -> Self {
        let path = config::settings_path();
        let cfg = config::load_file(Some(&path)).unwrap_or_default();
        let mut gui = Self {
            saved: cfg.clone(),
            cfg,
            path,
            changed_at: None,
            save_error: None,
            tab: Tab::Connections,
            quick_host: String::new(),
            new_conn: Connection::default(),
            editing: None,
            search: None,
            found: Vec::new(),
            searched: false,
            monitors: Vec::new(),
            local_ips: Vec::new(),
            sessions: Vec::new(),
        };
        gui.refresh_system_info();
        gui
    }

    fn refresh_system_info(&mut self) {
        self.monitors = capture::list_monitors()
            .map(|ms| ms.iter().map(ToString::to_string).collect())
            .unwrap_or_default();
        self.local_ips = discovery::local_ipv4().into_iter().map(|(name, ip, _)| (ip.to_string(), name)).collect();
    }

    /// Enregistre les réglages sur disque, au plus tard 0,5 s après la dernière modification.
    fn autosave(&mut self, force: bool) {
        if self.cfg != self.saved {
            let since = *self.changed_at.get_or_insert_with(Instant::now);
            if force || since.elapsed() > Duration::from_millis(500) {
                match config::save_file(&self.path, &self.cfg) {
                    Ok(()) => self.save_error = None,
                    Err(e) => self.save_error = Some(format!("{e:#}")),
                }
                self.saved = self.cfg.clone();
                self.changed_at = None;
            }
        } else {
            self.changed_at = None;
        }
    }

    fn spawn(&mut self, ctx: &egui::Context, kind: Kind, label: String, args: &[String]) {
        self.autosave(true);
        let exe = std::env::current_exe().unwrap_or_else(|_| "pip-pbp.exe".into());
        let mut cmd = Command::new(exe);
        cmd.arg("--config").arg(&self.path).args(args);
        cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
        cmd.creation_flags(CREATE_NO_WINDOW);

        let log = Arc::new(Mutex::new(VecDeque::new()));
        match cmd.spawn() {
            Ok(mut child) => {
                if let Some(stderr) = child.stderr.take() {
                    let (log, ctx) = (log.clone(), ctx.clone());
                    std::thread::spawn(move || {
                        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                            let mut l = log.lock().unwrap();
                            if l.len() >= MAX_LOG_LINES {
                                l.pop_front();
                            }
                            l.push_back(line);
                            drop(l);
                            ctx.request_repaint();
                        }
                    });
                }
                self.sessions.push(Session { kind, label, child: Some(child), log, exit: None });
            }
            Err(e) => {
                log.lock().unwrap().push_back(format!("Impossible de lancer le processus : {e}"));
                self.sessions.push(Session { kind, label, child: None, log, exit: Some("erreur".into()) });
            }
        }
    }

    fn connect(&mut self, ctx: &egui::Context, label: &str, host: &str, port: u16) {
        let args = vec!["view".into(), host.trim().to_string(), "--port".into(), port.to_string()];
        self.spawn(ctx, Kind::View, format!("Affichage de {label}"), &args);
    }

    fn sharing(&self) -> bool {
        self.sessions.iter().any(|s| s.kind == Kind::Send && s.is_running())
    }

    fn poll_sessions(&mut self) {
        for s in &mut self.sessions {
            if s.exit.is_none()
                && let Some(child) = &mut s.child
                && let Ok(Some(status)) = child.try_wait()
            {
                s.exit = Some(if status.success() { "terminée".into() } else { format!("arrêtée ({status})") });
            }
        }
        if let Some(rx) = &self.search
            && let Ok(found) = rx.try_recv()
        {
            self.found = found;
            self.search = None;
            self.searched = true;
        }
    }

    fn start_search(&mut self, ctx: &egui::Context) {
        let (tx, rx) = mpsc::channel();
        let (ctx, port) = (ctx.clone(), self.cfg.view.port);
        std::thread::spawn(move || {
            let _ = tx.send(discovery::find_sources(port, Duration::from_secs(3)));
            ctx.request_repaint();
        });
        self.search = Some(rx);
    }

    // ----- Onglet Connexions -------------------------------------------------------

    fn connections_tab(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();

        section(ui, "Sources détectées sur le réseau local");
        ui.horizontal(|ui| {
            let searching = self.search.is_some();
            if ui.add_enabled(!searching, egui::Button::new("🔍 Rechercher")).clicked() {
                self.start_search(&ctx);
            }
            if searching {
                ui.spinner();
                ui.label("Recherche en cours…");
            } else if self.searched && self.found.is_empty() {
                ui.label(RichText::new("Aucune source trouvée. Lancez « Partager cet écran » sur l'autre PC.").weak());
            }
        });
        let mut action = None;
        for (ip, name) in &self.found {
            ui.horizontal(|ui| {
                ui.label(RichText::new(name).strong());
                ui.label(ip.to_string());
                if ui.button("▶ Afficher").clicked() {
                    action = Some((name.clone(), ip.to_string(), false));
                }
                let known = self.cfg.connections.iter().any(|c| c.host == ip.to_string());
                if !known && ui.button("Enregistrer").clicked() {
                    action = Some((name.clone(), ip.to_string(), true));
                }
            });
        }
        if let Some((name, host, save)) = action {
            if save {
                self.cfg.connections.push(Connection { name, host, port: self.cfg.view.port });
            } else {
                let port = self.cfg.view.port;
                self.connect(&ctx, &name, &host, port);
            }
        }

        ui.add_space(12.0);
        section(ui, "Connexions enregistrées");
        if self.cfg.connections.is_empty() {
            ui.label(RichText::new("Aucune connexion enregistrée pour l'instant.").weak());
        }
        let mut remove = None;
        let mut connect = None;
        egui::Grid::new("connections").num_columns(4).spacing([12.0, 6.0]).striped(true).show(ui, |ui| {
            for (i, c) in self.cfg.connections.iter_mut().enumerate() {
                if self.editing == Some(i) {
                    ui.add(egui::TextEdit::singleline(&mut c.name).hint_text("Nom").desired_width(140.0));
                    ui.add(egui::TextEdit::singleline(&mut c.host).hint_text("Adresse IP").desired_width(140.0));
                    ui.add(egui::DragValue::new(&mut c.port).range(1..=65535));
                    if ui.button("✔ OK").clicked() {
                        self.editing = None;
                    }
                } else {
                    ui.label(RichText::new(&c.name).strong());
                    ui.label(&c.host);
                    ui.label(c.port.to_string());
                    ui.horizontal(|ui| {
                        if ui.button("▶ Afficher").clicked() {
                            connect = Some(c.clone());
                        }
                        if ui.button("Modifier").clicked() {
                            self.editing = Some(i);
                        }
                        if ui.button("Supprimer").clicked() {
                            remove = Some(i);
                        }
                    });
                }
                ui.end_row();
            }
        });
        if let Some(i) = remove {
            self.cfg.connections.remove(i);
            self.editing = None;
        }
        if let Some(c) = connect {
            let label = if c.name.is_empty() { c.host.clone() } else { c.name.clone() };
            self.connect(&ctx, &label, &c.host, c.port);
        }

        ui.add_space(12.0);
        section(ui, "Nouvelle connexion");
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.new_conn.name).hint_text("Nom (ex. PC bureau)").desired_width(150.0));
            ui.add(egui::TextEdit::singleline(&mut self.new_conn.host).hint_text("Adresse IP").desired_width(150.0));
            ui.label("Port");
            ui.add(egui::DragValue::new(&mut self.new_conn.port).range(1..=65535));
            let valid = !self.new_conn.host.trim().is_empty();
            if ui.add_enabled(valid, egui::Button::new("+ Enregistrer")).clicked() {
                let mut c = std::mem::take(&mut self.new_conn);
                c.host = c.host.trim().to_string();
                if c.name.trim().is_empty() {
                    c.name = c.host.clone();
                }
                self.cfg.connections.push(c);
                self.new_conn.port = self.cfg.view.port;
            }
        });

        ui.add_space(12.0);
        section(ui, "Connexion rapide");
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.quick_host).hint_text("Adresse IP").desired_width(150.0));
            let host = self.quick_host.trim().to_string();
            if ui.add_enabled(!host.is_empty(), egui::Button::new("▶ Afficher")).clicked() {
                let port = self.cfg.view.port;
                self.connect(&ctx, &host, &host, port);
            }
            if ui.button("▶ Détection automatique").on_hover_text("Se connecte à la première source trouvée").clicked() {
                let port = self.cfg.view.port;
                self.connect(&ctx, "la source détectée", "auto", port);
            }
        });
    }

    // ----- Onglet Partager ---------------------------------------------------------

    fn share_tab(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let running = self.sharing();

        section(ui, "Partager l'écran de ce PC");
        ui.label("Les autres PC du réseau pourront afficher cet écran dans une fenêtre PiP.");
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if running {
                if ui.add(egui::Button::new(RichText::new("■ Arrêter le partage").strong()).fill(Color32::from_rgb(150, 40, 40))).clicked() {
                    for s in self.sessions.iter_mut().filter(|s| s.kind == Kind::Send && s.is_running()) {
                        s.kill();
                    }
                }
                ui.label(RichText::new("● Partage actif").color(Color32::from_rgb(80, 200, 120)));
            } else if ui.add(egui::Button::new(RichText::new("▶ Démarrer le partage").strong()).fill(Color32::from_rgb(40, 110, 60))).clicked() {
                self.spawn(&ctx, Kind::Send, "Partage de cet écran".into(), &["send".into()]);
            }
        });
        ui.add_space(4.0);
        ui.label(RichText::new("La première fois, autorisez pip-pbp dans le pare-feu Windows (réseaux privés et publics).").weak());

        ui.add_space(12.0);
        section(ui, "Adresses de ce PC");
        for (ip, name) in &self.local_ips {
            ui.horizontal(|ui| {
                ui.monospace(ip);
                ui.label(RichText::new(name).weak());
            });
        }

        ui.add_space(12.0);
        section(ui, "Réglages du partage");
        ui.add_enabled_ui(!running, |ui| {
            let s = &mut self.cfg.send;
            egui::Grid::new("send").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
                ui.label("Écran capturé");
                ui.horizontal(|ui| {
                    let current = self.monitors.get(s.monitor).cloned().unwrap_or_else(|| format!("Écran {}", s.monitor));
                    egui::ComboBox::from_id_salt("monitor").selected_text(current).width(300.0).show_ui(ui, |ui| {
                        for (i, m) in self.monitors.iter().enumerate() {
                            ui.selectable_value(&mut s.monitor, i, m);
                        }
                    });
                });
                ui.end_row();

                ui.label("Images par seconde");
                ui.add(egui::Slider::new(&mut s.fps, 15..=144));
                ui.end_row();

                ui.label("Débit (Mbit/s)");
                ui.add(egui::Slider::new(&mut s.bitrate_mbps, 2..=100));
                ui.end_row();

                ui.label("Résolution");
                egui::ComboBox::from_id_salt("downscale")
                    .selected_text(downscale_label(s.downscale))
                    .show_ui(ui, |ui| {
                        for d in 0..=3 {
                            ui.selectable_value(&mut s.downscale, d, downscale_label(d));
                        }
                    });
                ui.end_row();

                ui.label("Curseur de la souris");
                ui.checkbox(&mut s.show_cursor, "Afficher le curseur de ce PC");
                ui.end_row();

                ui.label("Qualité du texte");
                ui.checkbox(&mut s.sharp_text, "Texte plus net (encodage ~3× plus lent)");
                ui.end_row();

                ui.label("Port");
                ui.add(egui::DragValue::new(&mut s.port).range(1..=65535));
                ui.end_row();
            });
        });
        if running {
            ui.label(RichText::new("Arrêtez le partage pour modifier ces réglages.").weak());
        }
        if ui.small_button("Actualiser la liste des écrans").clicked() {
            self.refresh_system_info();
        }
    }

    // ----- Onglet Affichage --------------------------------------------------------

    fn display_tab(&mut self, ui: &mut egui::Ui) {
        section(ui, "Fenêtre d'affichage (PiP / PbP)");
        ui.label(RichText::new("Appliqué à la prochaine connexion.").weak());
        ui.add_space(6.0);
        let v = &mut self.cfg.view;
        egui::Grid::new("view").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
            ui.label("Opacité au survol de la souris");
            percent_slider(ui, &mut v.hover_opacity, 0.0);
            ui.end_row();

            ui.label("Opacité normale");
            percent_slider(ui, &mut v.opacity, 0.1);
            ui.end_row();

            ui.label("Clic à travers");
            ui.checkbox(&mut v.click_through_on_hover, "Les clics traversent la fenêtre au survol (Alt = reprendre la main)");
            ui.end_row();

            ui.label("Premier plan");
            ui.checkbox(&mut v.always_on_top, "Toujours au-dessus des autres fenêtres");
            ui.end_row();

            ui.label("Proportions");
            ui.checkbox(&mut v.keep_aspect, "Garder le ratio de l'image (bandes noires)");
            ui.end_row();

            ui.label("Barre de titre");
            ui.checkbox(&mut v.decorations, "Afficher la barre de titre Windows");
            ui.end_row();

            ui.label("Disposition au démarrage");
            egui::ComboBox::from_id_salt("layout").selected_text(layout_label(&v.layout)).show_ui(ui, |ui| {
                for l in ["pip", "pbp-left", "pbp-right", "free"] {
                    ui.selectable_value(&mut v.layout, l.to_string(), layout_label(l));
                }
            });
            ui.end_row();

            ui.label("Taille du PiP");
            percent_slider(ui, &mut v.pip_scale, 0.1);
            ui.end_row();

            ui.label("Taille libre (pixels)");
            ui.horizontal(|ui| {
                ui.add(egui::DragValue::new(&mut v.width).range(160..=7680));
                ui.label("×");
                ui.add(egui::DragValue::new(&mut v.height).range(90..=4320));
            });
            ui.end_row();

            ui.label("Port par défaut");
            ui.add(egui::DragValue::new(&mut v.port).range(1..=65535));
            ui.end_row();
        });

        ui.add_space(12.0);
        section(ui, "Raccourcis dans la fenêtre");
        egui::Grid::new("keys").num_columns(2).spacing([16.0, 4.0]).show(ui, |ui| {
            for (k, d) in [
                ("Alt maintenu", "Fenêtre opaque et cliquable (déplacer / redimensionner)"),
                ("Molette", "Agrandir / réduire"),
                ("Double-clic, F, F11", "Plein écran"),
                ("1 / 2 / 3", "PiP coin / PbP gauche / PbP droite"),
                ("↑ / ↓", "Opacité au survol ±5 %"),
                ("C / T / A", "Clic à travers / premier plan / ratio"),
                ("Ctrl+Q", "Fermer la fenêtre"),
            ] {
                ui.monospace(k);
                ui.label(d);
                ui.end_row();
            }
        });

        ui.add_space(12.0);
        if ui.button("Réinitialiser les réglages d'affichage").clicked() {
            self.cfg.view = config::ViewConfig::default();
        }
    }

    fn sessions_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Sessions");
            if self.sessions.iter().any(|s| !s.is_running()) && ui.small_button("Effacer les terminées").clicked() {
                self.sessions.retain(Session::is_running);
            }
        });
        if self.sessions.is_empty() {
            ui.label(RichText::new("Aucune session. Choisissez une connexion ou partagez cet écran.").weak());
            return;
        }
        egui::ScrollArea::vertical().max_height(220.0).auto_shrink([false, true]).show(ui, |ui| {
            for (i, s) in self.sessions.iter_mut().enumerate().rev() {
                let (dot, color) = match &s.exit {
                    None => ("●", Color32::from_rgb(80, 200, 120)),
                    Some(_) => ("●", Color32::GRAY),
                };
                let log_handle = s.log.clone();
                let log = log_handle.lock().unwrap();
                let last = log.back().cloned().unwrap_or_default();
                let header = format!("{} — {}", s.label, s.exit.as_deref().unwrap_or("en cours"));
                ui.horizontal(|ui| {
                    ui.label(RichText::new(dot).color(color));
                    egui::CollapsingHeader::new(header).id_salt(("session", i)).show(ui, |ui| {
                        egui::ScrollArea::vertical().max_height(140.0).stick_to_bottom(true).show(ui, |ui| {
                            for line in log.iter() {
                                ui.monospace(line);
                            }
                        });
                    });
                    if s.exit.is_none() && ui.small_button("Arrêter").clicked() {
                        s.kill();
                    }
                });
                if !last.is_empty() {
                    ui.label(RichText::new(last).weak().small());
                }
            }
        });
    }
}

impl eframe::App for Gui {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll_sessions();

        egui::Panel::top("tabs").show(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading(RichText::new("PiP-PbP").strong());
                ui.add_space(16.0);
                ui.selectable_value(&mut self.tab, Tab::Connections, "Connexions");
                ui.selectable_value(&mut self.tab, Tab::Share, "Partager cet écran");
                ui.selectable_value(&mut self.tab, Tab::Display, "Réglages d'affichage");
            });
            ui.add_space(4.0);
        });

        egui::Panel::bottom("sessions").resizable(true).show(ui, |ui| {
            ui.add_space(4.0);
            self.sessions_panel(ui);
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                let text = match &self.save_error {
                    Some(e) => RichText::new(format!("Erreur d'enregistrement : {e}")).color(Color32::LIGHT_RED),
                    None => RichText::new(format!("Réglages enregistrés dans {}", self.path.display())).weak().small(),
                };
                ui.label(text);
            });
        });

        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| match self.tab {
                Tab::Connections => self.connections_tab(ui),
                Tab::Share => self.share_tab(ui),
                Tab::Display => self.display_tab(ui),
            });
        });

        self.autosave(false);
        // Rafraîchit l'état des sessions et l'enregistrement différé.
        ui.ctx().request_repaint_after(Duration::from_millis(500));
    }

    fn on_exit(&mut self) {
        self.autosave(true);
        // Les fenêtres PiP et le partage sont arrêtés avec l'interface.
        for s in &mut self.sessions {
            if s.exit.is_none() {
                s.kill();
            }
        }
    }
}

fn section(ui: &mut egui::Ui, title: &str) {
    ui.label(RichText::new(title).size(16.0).strong());
    ui.separator();
}

fn percent_slider(ui: &mut egui::Ui, value: &mut f32, min: f32) {
    let mut pct = (*value * 100.0).round() as i32;
    if ui.add(egui::Slider::new(&mut pct, (min * 100.0) as i32..=100).suffix(" %")).changed() {
        *value = pct as f32 / 100.0;
    }
}

fn downscale_label(d: u32) -> &'static str {
    match d {
        0 => "Automatique (4K → 1080p)",
        1 => "Native",
        2 => "Moitié",
        _ => "Tiers",
    }
}

fn layout_label(l: &str) -> &'static str {
    match l {
        "pip" => "PiP — coin bas-droit",
        "pbp-left" => "PbP — moitié gauche",
        "pbp-right" => "PbP — moitié droite",
        _ => "Libre",
    }
}
