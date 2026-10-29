//! Mesure de la latence de bout en bout : de la capture de l'écran sur la source jusqu'à
//! l'affichage dans la fenêtre PiP.
//!
//! Les deux PC n'ont pas la même horloge. La source horodate chaque image (capture, envoi)
//! avec la sienne ; le viewer mesure régulièrement le décalage entre les deux horloges par
//! un aller-retour (ping), comme NTP : décalage = milieu de l'aller-retour - heure source.

use std::collections::VecDeque;
use std::sync::OnceLock;
use std::time::Instant;

/// En-tête des paquets vidéo : instant de capture et instant d'envoi (µs, horloge source).
pub const VIDEO_HEADER: usize = 16;

static ORIGIN: OnceLock<Instant> = OnceLock::new();

/// Horloge monotone du processus, en microsecondes.
pub fn now_us() -> u64 {
    ORIGIN.get_or_init(Instant::now).elapsed().as_micros() as u64
}

/// Décalage entre l'horloge de la source et celle de ce PC.
#[derive(Default)]
pub struct ClockSync {
    /// (aller-retour, décalage) des derniers pings.
    samples: VecDeque<(u64, i64)>,
}

impl ClockSync {
    /// Ping envoyé à `t0`, réponse reçue à `t1` (horloge locale), heure source `ts`.
    pub fn add(&mut self, t0: u64, ts: u64, t1: u64) {
        if t1 < t0 {
            return;
        }
        let rtt = t1 - t0;
        let offset = ((t0 + t1) / 2) as i64 - ts as i64;
        self.samples.push_back((rtt, offset));
        if self.samples.len() > 16 {
            self.samples.pop_front();
        }
    }

    /// Décalage (heure locale = heure source + décalage) : celui du ping le plus rapide,
    /// le moins perturbé par le réseau.
    pub fn offset(&self) -> Option<i64> {
        self.samples.iter().min_by_key(|(rtt, _)| *rtt).map(|(_, o)| *o)
    }

    pub fn rtt_us(&self) -> Option<u64> {
        self.samples.iter().map(|(rtt, _)| *rtt).min()
    }
}

/// Horodatage d'une image : `capture` et `sent` à l'horloge de la source,
/// `received` et `decoded` à l'horloge locale.
#[derive(Clone, Copy, Debug)]
pub struct FrameTiming {
    pub capture: u64,
    pub sent: u64,
    pub received: u64,
    pub decoded: u64,
}

/// Moyennes glissantes (en ms) des étapes du trajet d'une image.
#[derive(Default)]
pub struct LatencyStats {
    total: f64,
    source: f64,
    network: f64,
    decode: f64,
    display: f64,
    samples: u64,
}

impl LatencyStats {
    /// Image affichée à `presented` (horloge locale).
    pub fn record(&mut self, t: &FrameTiming, presented: u64, offset: i64) {
        let ms = |us: i64| us.max(0) as f64 / 1000.0;
        let sent_local = t.sent as i64 + offset;
        let capture_local = t.capture as i64 + offset;
        let values = [
            ms(presented as i64 - capture_local),
            ms(t.sent as i64 - t.capture as i64),
            ms(t.received as i64 - sent_local),
            ms(t.decoded as i64 - t.received as i64),
            ms(presented as i64 - t.decoded as i64),
        ];
        // Moyenne glissante : réactive mais sans sauter à chaque image.
        let alpha = if self.samples == 0 { 1.0 } else { 0.1 };
        for (slot, v) in [&mut self.total, &mut self.source, &mut self.network, &mut self.decode, &mut self.display]
            .into_iter()
            .zip(values)
        {
            *slot += alpha * (v - *slot);
        }
        self.samples += 1;
    }

    pub fn has_data(&self) -> bool {
        self.samples > 0
    }

    /// « Latence 24 ms (source 9 · réseau 1 · décodage 4 · affichage 10) ».
    pub fn summary(&self) -> String {
        format!(
            "Latence {:.0} ms (source {:.1} · réseau {:.1} · décodage {:.1} · affichage {:.1})",
            self.total, self.source, self.network, self.decode, self.display
        )
    }

    /// Texte du compteur affiché sur la fenêtre PiP.
    pub fn overlay(&self) -> String {
        format!(
            "Latence  {:.0} ms\nsource {:.1} · réseau {:.1}\ndécodage {:.1} · affichage {:.1}",
            self.total, self.source, self.network, self.decode, self.display
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_offset_from_pings() {
        // Horloge locale en avance de 5 000 µs sur la source, réseau symétrique.
        let mut c = ClockSync::default();
        c.add(100_000, 95_000 + 400, 100_800); // aller-retour 800 µs
        c.add(200_000, 195_000 + 2_000, 204_000); // ping lent (4 ms), ignoré
        assert_eq!(c.offset(), Some(5_000));
        assert_eq!(c.rtt_us(), Some(800));
    }

    #[test]
    fn stages_add_up() {
        let mut s = LatencyStats::default();
        // Source : capture à 1 000, envoi à 9 000 (8 ms). Décalage local +5 000.
        // Réception locale 15 000 (1 ms de réseau), décodé 19 000, affiché 21 000.
        let t = FrameTiming { capture: 1_000, sent: 9_000, received: 15_000, decoded: 19_000 };
        s.record(&t, 21_000, 5_000);
        assert_eq!(s.summary(), "Latence 15 ms (source 8.0 · réseau 1.0 · décodage 4.0 · affichage 2.0)");
    }
}
