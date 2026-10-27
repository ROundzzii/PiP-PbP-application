//! PiP / PbP réseau : affiche l'écran d'un autre PC dans une fenêtre flottante,
//! avec une latence la plus faible possible.
//!
//! - `pip-pbp send` sur le PC dont on veut voir l'écran (capture + encodage H.264).
//! - `pip-pbp view <ip>` sur le PC qui affiche la fenêtre PiP/PbP.

mod capture;
mod config;
mod discovery;
mod protocol;
mod render;
mod sender;
mod viewer;
mod win;
mod yuv;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "pip-pbp", version, about = "Picture-in-Picture / Picture-by-Picture entre deux PC")]
struct Cli {
    /// Fichier de configuration TOML (par défaut : pip-pbp.toml à côté de l'exécutable ou dans le dossier courant)
    #[arg(long, global = true)]
    config: Option<std::path::PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Partage l'écran de ce PC (à lancer sur la machine source)
    Send(config::SendArgs),
    /// Affiche l'écran distant dans une fenêtre PiP/PbP
    View(config::ViewArgs),
    /// Liste les écrans disponibles pour la capture
    Monitors,
    /// Mesure les performances de capture/encodage de ce PC (sans réseau)
    Bench(config::SendArgs),
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let file = config::load_file(cli.config.as_deref())?;
    match cli.command {
        Command::Send(args) => sender::run(file.send.merge(args)),
        Command::View(args) => viewer::run(file.view.merge(args)),
        Command::Bench(args) => sender::bench(file.send.merge(args)),
        Command::Monitors => {
            for (i, m) in capture::list_monitors()?.iter().enumerate() {
                println!("[{i}] {m}");
            }
            Ok(())
        }
    }
}
