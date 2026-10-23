//! Configuration : fichier TOML optionnel + arguments en ligne de commande
//! (les arguments CLI ont priorité sur le fichier).

use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::Args;
use serde::Deserialize;

pub const DEFAULT_PORT: u16 = 47800;
pub const CONFIG_FILE_NAME: &str = "pip-pbp.toml";

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct FileConfig {
    pub send: SendConfig,
    pub view: ViewConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SendConfig {
    /// Port TCP d'écoute.
    pub port: u16,
    /// Index de l'écran à capturer (voir `pip-pbp monitors`).
    pub monitor: usize,
    /// Images par seconde maximum.
    pub fps: u32,
    /// Débit cible en Mbit/s.
    pub bitrate_mbps: u32,
    /// Facteur de réduction de la résolution (1 = natif, 2 = moitié).
    /// 0 = automatique : réduit seulement les écrans plus larges que 2560 px (4K -> 1080p).
    pub downscale: u32,
    /// Dessine le curseur de la souris du PC source dans l'image.
    pub show_cursor: bool,
    /// Mode d'encodage « texte » : plus net sur du texte fixe mais ~3× plus lent.
    pub sharp_text: bool,
}

impl Default for SendConfig {
    fn default() -> Self {
        Self { port: DEFAULT_PORT, monitor: 0, fps: 60, bitrate_mbps: 20, downscale: 0, show_cursor: true, sharp_text: false }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ViewConfig {
    /// Adresse IP ou nom du PC source.
    pub host: String,
    pub port: u16,
    /// Opacité normale de la fenêtre (0.0 – 1.0).
    pub opacity: f32,
    /// Opacité quand la souris passe sur la fenêtre (0.0 – 1.0).
    pub hover_opacity: f32,
    /// Quand la souris est sur la fenêtre, les clics passent à travers
    /// (maintenir Alt pour reprendre la main sur la fenêtre).
    pub click_through_on_hover: bool,
    /// Fenêtre toujours au premier plan.
    pub always_on_top: bool,
    /// Conserve le ratio de l'image (bandes noires sinon).
    pub keep_aspect: bool,
    /// Barre de titre Windows.
    pub decorations: bool,
    /// Taille initiale de la fenêtre (pixels physiques).
    pub width: u32,
    pub height: u32,
    /// Position initiale : "pip" (coin bas-droit), "pbp-left", "pbp-right" ou "free".
    pub layout: String,
    /// Fraction de la largeur de l'écran utilisée en mode PiP.
    pub pip_scale: f32,
}

impl Default for ViewConfig {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: DEFAULT_PORT,
            opacity: 1.0,
            hover_opacity: 0.3,
            click_through_on_hover: true,
            always_on_top: true,
            keep_aspect: true,
            decorations: false,
            width: 960,
            height: 540,
            layout: "pip".into(),
            pip_scale: 0.3,
        }
    }
}

#[derive(Debug, Args)]
pub struct SendArgs {
    #[arg(short, long)]
    pub port: Option<u16>,
    #[arg(short, long)]
    pub monitor: Option<usize>,
    #[arg(long)]
    pub fps: Option<u32>,
    #[arg(short, long = "bitrate")]
    pub bitrate_mbps: Option<u32>,
    #[arg(long)]
    pub downscale: Option<u32>,
    #[arg(long)]
    pub no_cursor: bool,
    /// Encodage optimisé pour le texte (plus lent)
    #[arg(long)]
    pub sharp_text: bool,
}

#[derive(Debug, Args)]
pub struct ViewArgs {
    /// Adresse IP du PC source
    pub host: Option<String>,
    #[arg(short, long)]
    pub port: Option<u16>,
    /// Opacité au survol de la souris (0.0 – 1.0)
    #[arg(long)]
    pub hover_opacity: Option<f32>,
    /// Opacité normale (0.0 – 1.0)
    #[arg(long)]
    pub opacity: Option<f32>,
    /// pip | pbp-left | pbp-right | free
    #[arg(long)]
    pub layout: Option<String>,
    /// Désactive le clic à travers la fenêtre au survol
    #[arg(long)]
    pub no_click_through: bool,
}

impl SendConfig {
    pub fn merge(mut self, a: SendArgs) -> Self {
        if let Some(v) = a.port { self.port = v; }
        if let Some(v) = a.monitor { self.monitor = v; }
        if let Some(v) = a.fps { self.fps = v; }
        if let Some(v) = a.bitrate_mbps { self.bitrate_mbps = v; }
        if let Some(v) = a.downscale { self.downscale = v; }
        if a.no_cursor { self.show_cursor = false; }
        if a.sharp_text { self.sharp_text = true; }
        self.fps = self.fps.clamp(1, 240);
        self.downscale = self.downscale.min(4);
        self
    }
}

impl ViewConfig {
    pub fn merge(mut self, a: ViewArgs) -> Self {
        if let Some(v) = a.host { self.host = v; }
        if let Some(v) = a.port { self.port = v; }
        if let Some(v) = a.hover_opacity { self.hover_opacity = v; }
        if let Some(v) = a.opacity { self.opacity = v; }
        if let Some(v) = a.layout { self.layout = v; }
        if a.no_click_through { self.click_through_on_hover = false; }
        self.opacity = self.opacity.clamp(0.05, 1.0);
        self.hover_opacity = self.hover_opacity.clamp(0.0, 1.0);
        self.pip_scale = self.pip_scale.clamp(0.05, 1.0);
        self
    }
}

pub fn load_file(explicit: Option<&Path>) -> anyhow::Result<FileConfig> {
    let path = match explicit {
        Some(p) => Some(p.to_path_buf()),
        None => default_locations().into_iter().find(|p| p.exists()),
    };
    let Some(path) = path else { return Ok(FileConfig::default()) };
    let text = std::fs::read_to_string(&path).with_context(|| format!("lecture de {}", path.display()))?;
    let cfg = toml::from_str(&text).with_context(|| format!("format invalide dans {}", path.display()))?;
    eprintln!("Configuration chargée depuis {}", path.display());
    Ok(cfg)
}

fn default_locations() -> Vec<PathBuf> {
    let mut v = vec![PathBuf::from(CONFIG_FILE_NAME)];
    if let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)) {
        v.push(dir.join(CONFIG_FILE_NAME));
    }
    v
}
