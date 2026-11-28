//! Numbers drawn straight into a tray icon (port of trayicon.js), RGBA out.

const GLYPHS: [(char, [&str; 5]); 11] = [
    ('0', ["111", "101", "101", "101", "111"]),
    ('1', ["010", "110", "010", "010", "111"]),
    ('2', ["111", "001", "111", "100", "111"]),
    ('3', ["111", "001", "111", "001", "111"]),
    ('4', ["101", "101", "111", "001", "001"]),
    ('5', ["111", "100", "111", "001", "111"]),
    ('6', ["111", "100", "111", "101", "111"]),
    ('7', ["111", "001", "010", "010", "010"]),
    ('8', ["111", "101", "111", "101", "111"]),
    ('9', ["111", "101", "111", "001", "111"]),
    ('!', ["010", "010", "010", "000", "010"]),
];

fn colour(role: &str) -> [u8; 3] {
    match role {
        "warn" => [0xaa, 0x62, 0x00],
        "danger" => [0xb3, 0x26, 0x1e],
        "stale" => [0x5f, 0x63, 0x68],
        "context" => [0x0a, 0x84, 0x8f],
        _ => [0xb4, 0x4b, 0x2c],
    }
}

struct Bitmap {
    size: usize,
    data: Vec<u8>,
}

impl Bitmap {
    fn set(&mut self, x: i32, y: i32, rgb: [u8; 3], a: u8) {
        if x < 0 || y < 0 || x as usize >= self.size || y as usize >= self.size {
            return;
        }
        let i = (y as usize * self.size + x as usize) * 4;
        let sa = a as f64 / 255.0;
        let da = self.data[i + 3] as f64 / 255.0;
        let oa = sa + da * (1.0 - sa);
        if oa == 0.0 {
            return;
        }
        for c in 0..3 {
            let dst = self.data[i + c] as f64;
            self.data[i + c] = ((rgb[c] as f64 * sa + dst * da * (1.0 - sa)) / oa).round() as u8;
        }
        self.data[i + 3] = (oa * 255.0).round() as u8;
    }

    fn rounded_rect(&mut self, x0: f64, y0: f64, x1: f64, y1: f64, r: f64, rgb: [u8; 3]) {
        for y in y0.floor() as i32..y1.ceil() as i32 {
            for x in x0.floor() as i32..x1.ceil() as i32 {
                let mut hits = 0u32;
                for sy in 0..3 {
                    for sx in 0..3 {
                        let px = x as f64 + (sx as f64 + 0.5) / 3.0;
                        let py = y as f64 + (sy as f64 + 0.5) / 3.0;
                        if px < x0 || px > x1 || py < y0 || py > y1 {
                            continue;
                        }
                        let dx = px - px.clamp(x0 + r, x1 - r);
                        let dy = py - py.clamp(y0 + r, y1 - r);
                        if dx * dx + dy * dy <= r * r + 0.01 {
                            hits += 1;
                        }
                    }
                }
                if hits > 0 {
                    self.set(x, y, rgb, ((255 * hits) as f64 / 9.0).round() as u8);
                }
            }
        }
    }
}

pub fn digits_for(pct: f64) -> String {
    let n = pct.round().clamp(0.0, 999.0) as u32;
    if n >= 100 { "!!".into() } else { n.to_string() }
}

/// 32x32 RGBA badge: filled colour block with white numerals.
pub fn render(pct: f64, role: &str) -> Vec<u8> {
    let size = 32usize;
    let s = size as f64;
    let mut bmp = Bitmap { size, data: vec![0; size * size * 4] };
    bmp.rounded_rect(0.5, 0.5, s - 0.5, s - 0.5, s * 0.28, colour(role));
    let text = digits_for(pct);
    let len = text.len() as f64;
    let gap = (s * 0.06).round().max(1.0);
    let per_glyph = (s * 0.9 - (len - 1.0) * gap) / len;
    let scale = ((per_glyph / 3.0).floor().min((s * 0.72 / 5.0).floor())).max(1.0) as i32;
    let glyph_w = 3 * scale;
    let total_w = text.len() as i32 * glyph_w + (text.len() as i32 - 1) * gap as i32;
    let mut x = ((s - total_w as f64) / 2.0).round() as i32;
    let y = ((s - 5.0 * scale as f64) / 2.0).round() as i32;
    for ch in text.chars() {
        if let Some((_, rows)) = GLYPHS.iter().find(|g| g.0 == ch) {
            for (gy, row) in rows.iter().enumerate() {
                for (gx, bit) in row.bytes().enumerate() {
                    if bit != b'1' {
                        continue;
                    }
                    for sy in 0..scale {
                        for sx in 0..scale {
                            bmp.set(x + gx as i32 * scale + sx, y + gy as i32 * scale + sy, [255; 3], 255);
                        }
                    }
                }
            }
        }
        x += glyph_w + gap as i32;
    }
    bmp.data
}

#[cfg(test)]
mod tests {
    #[test]
    fn badge_is_32x32_rgba_and_opaque_centre() {
        let d = super::render(42.0, "normal");
        assert_eq!(d.len(), 32 * 32 * 4);
        assert_eq!(d[(16 * 32 + 1) * 4 + 3], 255);
        assert_eq!(super::digits_for(100.0), "!!");
    }
}
