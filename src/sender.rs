//! Côté source : capture -> conversion YUV -> H.264 -> TCP.

use std::io::BufWriter;
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::Context;
use openh264::OpenH264API;
use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig, FrameRate, IntraFramePeriod, RateControlMode, UsageType,
};

use crate::capture::{Capturer, FrameStatus};
use crate::config::SendConfig;
use crate::discovery;
use crate::protocol::{
    self, IN_KEY, IN_MOUSE_BUTTON, IN_MOUSE_MOVE, IN_WHEEL, MAGIC, MSG_AUTH, MSG_HELLO, MSG_REPLACED, MSG_VIDEO,
    REQ_AUTH, REQ_KEYFRAME,
};
use crate::win::{self, Injector};
use crate::yuv::I420;

/// Nombre max de paquets en attente d'envoi. Au-delà, on saute des images plutôt que
/// d'accumuler du retard dans les buffers TCP : la latence reste constante.
const MAX_IN_FLIGHT: usize = 2;
/// Essais de code faux tolérés par connexion avant de la couper.
const MAX_AUTH_FAILURES: u32 = 5;

pub fn run(mut cfg: SendConfig) -> anyhow::Result<()> {
    // Pixels physiques partout : capture, et positions de souris du contrôle à distance.
    win::make_dpi_aware();
    if cfg.allow_control {
        if cfg.control_code.trim().len() < 4 {
            cfg.control_code = protocol::new_code();
        }
        eprintln!("Contrôle à distance AUTORISÉ, code : {}", cfg.control_code);
    } else {
        eprintln!("Contrôle à distance désactivé (affichage seul).");
    }
    let listener = TcpListener::bind(("0.0.0.0", cfg.port)).with_context(|| format!("écoute sur le port {}", cfg.port))?;
    discovery::spawn_responder(cfg.port);
    eprintln!("En attente d'un viewer sur le port {} (Ctrl+C pour quitter).", cfg.port);
    eprintln!("Sur l'autre PC, lancez `pip-pbp view` (détection automatique) ou :");
    for (name, ip, _) in discovery::local_ipv4() {
        eprintln!("    pip-pbp view {ip}    ({name})");
    }
    let mut capturer = Capturer::new(cfg.monitor, cfg.show_cursor)?;

    // Les connexions arrivent dans un thread à part : un nouveau viewer remplace
    // l'actuel au lieu d'attendre indéfiniment (écran noir) qu'il se déconnecte.
    let (conn_tx, conn_rx) = mpsc::channel::<TcpStream>();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(s) => {
                    if conn_tx.send(s).is_err() {
                        break;
                    }
                }
                Err(e) => eprintln!("Connexion refusée : {e}"),
            }
        }
    });

    let mut next = conn_rx.recv().ok();
    while let Some(stream) = next.take() {
        let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_default();
        eprintln!("Viewer connecté : {peer}");
        match stream_to(stream, &cfg, &mut capturer, &conn_rx) {
            Ok(Some(newer)) => {
                eprintln!("Viewer {peer} remplacé par une nouvelle connexion");
                next = Some(newer);
                continue;
            }
            Ok(None) => eprintln!("Viewer {peer} déconnecté"),
            Err(e) => eprintln!("Viewer {peer} déconnecté : {e:#}"),
        }
        next = conn_rx.recv().ok();
    }
    Ok(())
}

/// Envoie la vidéo jusqu'à la déconnexion du viewer, ou jusqu'à l'arrivée d'un
/// nouveau viewer (renvoyé pour être servi à la place).
fn stream_to(
    stream: TcpStream,
    cfg: &SendConfig,
    capturer: &mut Capturer,
    newer: &mpsc::Receiver<TcpStream>,
) -> anyhow::Result<Option<TcpStream>> {
    stream.set_nodelay(true)?;
    let mut reader = stream.try_clone()?;
    let reader_shutdown = stream.try_clone()?;
    let mut out = BufWriter::with_capacity(1 << 20, stream);
    std::io::Write::write_all(&mut out, MAGIC)?;
    // HELLO : contrôle autorisé ? + nombre aléatoire pour la preuve du code.
    let nonce = protocol::new_nonce();
    let mut hello = vec![u8::from(cfg.allow_control)];
    hello.extend_from_slice(&nonce);
    protocol::write_packet(&mut out, MSG_HELLO, &hello)?;
    // Écriture partagée : vidéo (thread réseau) et réponses au code (thread de lecture).
    let writer = Arc::new(Mutex::new(out));

    let keyframe_requested = Arc::new(AtomicBool::new(true));
    let alive = Arc::new(AtomicBool::new(true));

    // Envoi réseau dans un thread séparé pour que l'encodage n'attende jamais le réseau.
    let in_flight = Arc::new(AtomicUsize::new(0));
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let net = {
        let in_flight = in_flight.clone();
        let alive = alive.clone();
        let writer = writer.clone();
        std::thread::spawn(move || {
            for packet in rx {
                let r = protocol::write_packet(&mut *writer.lock().unwrap(), MSG_VIDEO, &packet);
                in_flight.fetch_sub(1, Ordering::Relaxed);
                if r.is_err() {
                    alive.store(false, Ordering::Relaxed);
                    break;
                }
            }
        })
    };

    // Requêtes du viewer : image clé, code de contrôle, souris et clavier.
    {
        let keyframe_requested = keyframe_requested.clone();
        let alive = alive.clone();
        let writer = writer.clone();
        let allow = cfg.allow_control;
        let code = cfg.control_code.clone();
        let injector = Injector::new(capturer.monitor_rect);
        std::thread::spawn(move || {
            let mut authed = false;
            let mut failures = 0;
            // Diagnostic : entrées appliquées / refusées par Windows, affiché toutes les 3 s.
            let mut applied = [0u32; 3];
            let mut refused = 0u32;
            let mut last_report = Instant::now();
            while let Ok((kind, p)) = protocol::read_request(&mut reader) {
                if last_report.elapsed() >= Duration::from_secs(3) && applied.iter().sum::<u32>() + refused > 0 {
                    eprintln!(
                        "Contrôle : {} mouvements, {} clics/molette, {} touches appliqués ; {} refusés par Windows",
                        applied[0], applied[1], applied[2], refused
                    );
                    if refused > 0 {
                        eprintln!("  (refus = la fenêtre active de ce PC est lancée en administrateur ou c'est un écran protégé)");
                    }
                    applied = [0; 3];
                    refused = 0;
                    last_report = Instant::now();
                }
                let mut count = |slot: usize, ok: bool| if ok { applied[slot] += 1 } else { refused += 1 };
                match kind {
                    REQ_KEYFRAME => keyframe_requested.store(true, Ordering::Relaxed),
                    REQ_AUTH => {
                        authed = allow && protocol::check_auth(&code, &nonce, &p);
                        let _ = protocol::write_packet(&mut *writer.lock().unwrap(), MSG_AUTH, &[u8::from(authed)]);
                        if authed {
                            eprintln!("Contrôle à distance : code accepté");
                        } else if allow {
                            failures += 1;
                            eprintln!("Contrôle à distance : code refusé ({failures}/{MAX_AUTH_FAILURES})");
                            if failures >= MAX_AUTH_FAILURES {
                                break;
                            }
                        }
                    }
                    // Les entrées ne sont appliquées qu'après validation du code.
                    _ if !authed => {}
                    IN_MOUSE_MOVE if p.len() == 4 => {
                        count(0, injector.move_to(u16::from_le_bytes([p[0], p[1]]), u16::from_le_bytes([p[2], p[3]])))
                    }
                    IN_MOUSE_BUTTON if p.len() == 2 => count(1, injector.button(p[0], p[1] != 0)),
                    IN_WHEEL if p.len() == 4 => {
                        count(1, injector.wheel(i16::from_le_bytes([p[0], p[1]]), i16::from_le_bytes([p[2], p[3]])))
                    }
                    IN_KEY if p.len() == 3 => count(2, injector.key(u16::from_le_bytes([p[0], p[1]]), p[2] != 0)),
                    _ => {}
                }
            }
            alive.store(false, Ordering::Relaxed);
        });
    }

    let mut encoder = Encoder::with_api_config(OpenH264API::from_source(), encoder_config(cfg))?;

    let interval = Duration::from_secs_f64(1.0 / cfg.fps as f64);
    let start = Instant::now();
    let mut next_allowed = Instant::now();
    let mut dirty = true;
    let mut yuv = I420::new();
    let mut stats = Stats::new();

    let mut replaced_by = None;
    while alive.load(Ordering::Relaxed) {
        if let Ok(s) = newer.try_recv() {
            replaced_by = Some(s);
            break;
        }
        // Image en attente : on n'attend que jusqu'au prochain créneau d'envoi.
        let wait = if dirty { next_allowed.saturating_duration_since(Instant::now()) } else { interval };
        if let FrameStatus::Updated = capturer.next_frame(wait.as_millis().max(1) as u32)? {
            dirty = true;
        }
        let force_key = keyframe_requested.load(Ordering::Relaxed);
        if !(dirty || force_key) || Instant::now() < next_allowed {
            continue;
        }
        if in_flight.load(Ordering::Relaxed) >= MAX_IN_FLIGHT {
            stats.skipped += 1;
            continue;
        }

        let t0 = Instant::now();
        yuv.convert_from_bgra(&capturer.frame, capturer.width, capturer.height, capturer.width * 4, downscale(cfg, capturer.width));
        if keyframe_requested.swap(false, Ordering::Relaxed) {
            encoder.force_intra_frame();
        }
        let ts = openh264::Timestamp::from_millis(start.elapsed().as_millis() as u64);
        let packet = encoder.encode_at(&yuv, ts)?.to_vec();
        stats.encode_ms += t0.elapsed().as_secs_f64() * 1000.0;

        dirty = false;
        next_allowed = t0 + interval;
        if packet.is_empty() {
            continue;
        }
        stats.frames += 1;
        stats.bytes += packet.len();
        in_flight.fetch_add(1, Ordering::Relaxed);
        if tx.send(packet).is_err() {
            break;
        }
        stats.maybe_print(yuv.width, yuv.height);
    }
    drop(tx);
    let _ = net.join();
    // Prévient l'ancien viewer qu'il est remplacé (il se ferme au lieu de se reconnecter).
    if replaced_by.is_some() {
        let _ = protocol::write_packet(&mut *writer.lock().unwrap(), MSG_REPLACED, &[]);
    }
    let _ = reader_shutdown.shutdown(std::net::Shutdown::Both);
    Ok(replaced_by)
}

/// Mesure les temps de conversion et d'encodage sans réseau (`pip-pbp bench`).
pub fn bench(cfg: SendConfig) -> anyhow::Result<()> {
    let mut capturer = Capturer::new(cfg.monitor, cfg.show_cursor)?;
    let _ = capturer.next_frame(500)?;
    let mut encoder = Encoder::with_api_config(OpenH264API::from_source(), encoder_config(&cfg))?;
    let mut yuv = I420::new();
    let (mut conv, mut enc, mut bytes) = (0.0, 0.0, 0usize);
    let n = 60;
    for i in 0..n {
        let _ = capturer.next_frame(1)?;
        let t0 = Instant::now();
        yuv.convert_from_bgra(&capturer.frame, capturer.width, capturer.height, capturer.width * 4, downscale(&cfg, capturer.width));
        let t1 = Instant::now();
        let len = encoder.encode_at(&yuv, openh264::Timestamp::from_millis(i * 16))?.to_vec().len();
        let t2 = Instant::now();
        conv += (t1 - t0).as_secs_f64() * 1000.0;
        enc += (t2 - t1).as_secs_f64() * 1000.0;
        bytes += len;
    }
    let n = n as f64;
    eprintln!(
        "{}x{} : conversion {:.1} ms, encodage {:.1} ms, {:.0} Ko/img en moyenne",
        yuv.width, yuv.height, conv / n, enc / n, bytes as f64 / n / 1024.0
    );
    Ok(())
}

/// Facteur de réduction effectif (mode automatique si `downscale = 0`).
fn downscale(cfg: &SendConfig, width: usize) -> usize {
    match cfg.downscale {
        0 => width.div_ceil(2560).max(1),
        n => n as usize,
    }
}

fn encoder_config(cfg: &SendConfig) -> EncoderConfig {
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(1, 4) as u16;
    // Le mode « screen content » d'OpenH264 est ~3× plus lent : on ne l'active que sur demande.
    let usage = if cfg.sharp_text { UsageType::ScreenContentRealTime } else { UsageType::CameraVideoRealTime };
    EncoderConfig::new()
        .usage_type(usage)
        .rate_control_mode(RateControlMode::Bitrate)
        .bitrate(BitRate::from_bps(cfg.bitrate_mbps.saturating_mul(1_000_000)))
        .max_frame_rate(FrameRate::from_hz(cfg.fps as f32))
        .skip_frames(false)
        .complexity(Complexity::Low)
        .intra_frame_period(IntraFramePeriod::from_num_frames(cfg.fps * 10))
        .num_threads(threads)
}

struct Stats {
    since: Instant,
    frames: u32,
    skipped: u32,
    bytes: usize,
    encode_ms: f64,
}

impl Stats {
    fn new() -> Self {
        Self { since: Instant::now(), frames: 0, skipped: 0, bytes: 0, encode_ms: 0.0 }
    }

    fn maybe_print(&mut self, w: usize, h: usize) {
        let dt = self.since.elapsed().as_secs_f64();
        if dt < 5.0 {
            return;
        }
        eprintln!(
            "{w}x{h} | {:.0} img/s | {:.1} Mbit/s | encodage {:.1} ms/img | {} images sautées (réseau lent)",
            self.frames as f64 / dt,
            self.bytes as f64 * 8.0 / dt / 1e6,
            self.encode_ms / self.frames.max(1) as f64,
            self.skipped,
        );
        *self = Self::new();
    }
}
