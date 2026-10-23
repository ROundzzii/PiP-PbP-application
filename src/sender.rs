//! Côté source : capture -> conversion YUV -> H.264 -> TCP.

use std::io::{BufWriter, Read};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
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
use crate::protocol::{self, MAGIC, MSG_VIDEO, REQ_KEYFRAME};
use crate::yuv::I420;

/// Nombre max de paquets en attente d'envoi. Au-delà, on saute des images plutôt que
/// d'accumuler du retard dans les buffers TCP : la latence reste constante.
const MAX_IN_FLIGHT: usize = 2;

pub fn run(cfg: SendConfig) -> anyhow::Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", cfg.port)).with_context(|| format!("écoute sur le port {}", cfg.port))?;
    eprintln!("En attente d'un viewer sur le port {} (Ctrl+C pour quitter)…", cfg.port);
    let mut capturer = Capturer::new(cfg.monitor, cfg.show_cursor)?;

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Connexion refusée : {e}");
                continue;
            }
        };
        let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_default();
        eprintln!("Viewer connecté : {peer}");
        match stream_to(stream, &cfg, &mut capturer) {
            Ok(()) => eprintln!("Viewer {peer} déconnecté"),
            Err(e) => eprintln!("Viewer {peer} déconnecté : {e:#}"),
        }
    }
    Ok(())
}

fn stream_to(stream: TcpStream, cfg: &SendConfig, capturer: &mut Capturer) -> anyhow::Result<()> {
    stream.set_nodelay(true)?;
    let mut reader = stream.try_clone()?;
    let mut writer = BufWriter::with_capacity(1 << 20, stream);
    std::io::Write::write_all(&mut writer, MAGIC)?;

    let keyframe_requested = Arc::new(AtomicBool::new(true));
    let alive = Arc::new(AtomicBool::new(true));

    // Lecture des requêtes du viewer.
    {
        let keyframe_requested = keyframe_requested.clone();
        let alive = alive.clone();
        std::thread::spawn(move || {
            let mut b = [0u8; 1];
            while reader.read_exact(&mut b).is_ok() {
                if b[0] == REQ_KEYFRAME {
                    keyframe_requested.store(true, Ordering::Relaxed);
                }
            }
            alive.store(false, Ordering::Relaxed);
        });
    }

    // Envoi réseau dans un thread séparé pour que l'encodage n'attende jamais le réseau.
    let in_flight = Arc::new(AtomicUsize::new(0));
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let net = {
        let in_flight = in_flight.clone();
        let alive = alive.clone();
        std::thread::spawn(move || {
            for packet in rx {
                let r = protocol::write_packet(&mut writer, MSG_VIDEO, &packet);
                in_flight.fetch_sub(1, Ordering::Relaxed);
                if r.is_err() {
                    alive.store(false, Ordering::Relaxed);
                    break;
                }
            }
        })
    };

    let mut encoder = Encoder::with_api_config(OpenH264API::from_source(), encoder_config(cfg))?;

    let interval = Duration::from_secs_f64(1.0 / cfg.fps as f64);
    let start = Instant::now();
    let mut next_allowed = Instant::now();
    let mut dirty = true;
    let mut yuv = I420::new();
    let mut stats = Stats::new();

    while alive.load(Ordering::Relaxed) {
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
    Ok(())
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
