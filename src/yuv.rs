//! Conversion BGRA -> I420 (BT.601 « limited range », ce qu'attend OpenH264),
//! parallélisée par bandes de lignes.

use openh264::formats::YUVSource;

pub struct I420 {
    pub width: usize,
    pub height: usize,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

impl I420 {
    pub fn new() -> Self {
        Self { width: 0, height: 0, y: Vec::new(), u: Vec::new(), v: Vec::new() }
    }

    fn resize(&mut self, width: usize, height: usize) {
        if self.width != width || self.height != height {
            self.width = width;
            self.height = height;
            self.y = vec![0; width * height];
            self.u = vec![0; width * height / 4];
            self.v = vec![0; width * height / 4];
        }
    }

    /// Convertit une image BGRA (`pitch` octets par ligne) en I420.
    /// `scale` réduit la résolution d'un facteur entier (échantillonnage simple).
    /// La taille de sortie est arrondie à un nombre pair (exigence du format 4:2:0).
    pub fn convert_from_bgra(&mut self, src: &[u8], src_w: usize, src_h: usize, pitch: usize, scale: usize) {
        let w = (src_w / scale) & !1;
        let h = (src_h / scale) & !1;
        self.resize(w, h);
        if w == 0 || h == 0 {
            return;
        }

        let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(1, 8);
        // Bandes d'un nombre pair de lignes pour que chaque bande ait ses lignes chroma.
        let band = (h / threads + 2) & !1;
        let (yb, ub, vb) = (&mut self.y, &mut self.u, &mut self.v);

        std::thread::scope(|s| {
            let y_chunks = yb.chunks_mut(band * w);
            let u_chunks = ub.chunks_mut(band / 2 * w / 2);
            let v_chunks = vb.chunks_mut(band / 2 * w / 2);
            for (i, ((yc, uc), vc)) in y_chunks.zip(u_chunks).zip(v_chunks).enumerate() {
                s.spawn(move || {
                    let row0 = i * band;
                    let rows = yc.len() / w;
                    for r in (0..rows).step_by(2) {
                        let sy0 = (row0 + r) * scale;
                        let sy1 = (row0 + r + 1) * scale;
                        let line0 = &src[sy0 * pitch..];
                        let line1 = &src[sy1 * pitch..];
                        let (ya, yb) = yc[r * w..(r + 2) * w].split_at_mut(w);
                        let crow = r / 2 * (w / 2);
                        for x in (0..w).step_by(2) {
                            let (mut sr, mut sg, mut sb) = (0i32, 0i32, 0i32);
                            let mut luma = |line: &[u8], px: usize| -> u8 {
                                let o = px * scale * 4;
                                let (b, g, r) = (line[o] as i32, line[o + 1] as i32, line[o + 2] as i32);
                                sr += r;
                                sg += g;
                                sb += b;
                                (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8
                            };
                            ya[x] = luma(line0, x);
                            ya[x + 1] = luma(line0, x + 1);
                            yb[x] = luma(line1, x);
                            yb[x + 1] = luma(line1, x + 1);
                            let (r, g, b) = (sr >> 2, sg >> 2, sb >> 2);
                            uc[crow + x / 2] = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128) as u8;
                            vc[crow + x / 2] = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128) as u8;
                        }
                    }
                });
            }
        });
    }
}

impl YUVSource for I420 {
    fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }
    fn strides(&self) -> (usize, usize, usize) {
        (self.width, self.width / 2, self.width / 2)
    }
    fn y(&self) -> &[u8] {
        &self.y
    }
    fn u(&self) -> &[u8] {
        &self.u
    }
    fn v(&self) -> &[u8] {
        &self.v
    }
}
