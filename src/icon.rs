//! The Ficherón icon, drawn in code: a teal rounded square with a white open folder and a
//! small spark. One drawing serves the window icon and the exported PNG/ICO files.

use std::path::Path;

fn sd_round_rect(px: f32, py: f32, cx: f32, cy: f32, hw: f32, hh: f32, r: f32) -> f32 {
    let (qx, qy) = ((px - cx).abs() - (hw - r), (py - cy).abs() - (hh - r));
    (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - r
}

fn mix(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t]
}

/// Colour at a point of the unit square (0..1), with alpha.
fn shade(x: f32, y: f32) -> [f32; 4] {
    // Background: rounded square, diagonal teal gradient.
    let bg = sd_round_rect(x, y, 0.5, 0.5, 0.5, 0.5, 0.22);
    if bg > 0.0 {
        return [0.0; 4];
    }
    let mut c = mix([0.09, 0.62, 0.62], [0.04, 0.30, 0.45], (x * 0.4 + y * 0.6).clamp(0.0, 1.0));
    // Folder back with its tab.
    let back = sd_round_rect(x, y, 0.5, 0.56, 0.30, 0.20, 0.04);
    let tab = sd_round_rect(x, y, 0.36, 0.33, 0.12, 0.05, 0.03);
    if back < 0.0 || tab < 0.0 {
        c = [0.78, 0.93, 0.95];
    }
    // Folder front flap, slightly lower and brighter.
    let front = sd_round_rect(x, y, 0.5, 0.62, 0.30, 0.15, 0.04);
    if front < 0.0 {
        c = mix([1.0, 1.0, 1.0], [0.86, 0.96, 0.97], ((y - 0.47) / 0.3).clamp(0.0, 1.0));
    }
    // Spark: a small amber diamond at the top right.
    let (dx, dy) = ((x - 0.74).abs(), (y - 0.26).abs());
    if dx + dy < 0.08 || (dx < 0.012 && dy < 0.12) || (dy < 0.012 && dx < 0.12) {
        c = [1.0, 0.78, 0.22];
    }
    [c[0], c[1], c[2], 1.0]
}

/// RGBA pixels of a `size` × `size` icon, with 3×3 supersampling for smooth edges.
pub fn rgba(size: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for j in 0..size {
        for i in 0..size {
            let mut acc = [0.0f32; 4];
            for sj in 0..3 {
                for si in 0..3 {
                    let s = shade((i as f32 + (si as f32 + 0.5) / 3.0) / size as f32, (j as f32 + (sj as f32 + 0.5) / 3.0) / size as f32);
                    for k in 0..3 {
                        acc[k] += s[k] * s[3];
                    }
                    acc[3] += s[3];
                }
            }
            let a = acc[3] / 9.0;
            for k in 0..3 {
                out.push(if acc[3] > 0.0 { (acc[k] / acc[3] * 255.0).round() as u8 } else { 0 });
            }
            out.push((a * 255.0).round() as u8);
        }
    }
    out
}

pub fn window_icon() -> eframe::egui::IconData {
    eframe::egui::IconData { rgba: rgba(256), width: 256, height: 256 }
}

/// Writes the PNG sizes and the multi-size Windows `.ico` into `dir` (for packaging).
pub fn export(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    for s in [16u32, 32, 48, 64, 128, 256, 512] {
        let img = image::RgbaImage::from_raw(s, s, rgba(s)).ok_or("icon")?;
        img.save(dir.join(format!("icon-{s}.png"))).map_err(|e| e.to_string())?;
    }
    // The ico encoder in `image` takes one image per file; build a multi-size file by hand
    // with PNG-compressed entries (supported since Windows Vista).
    let sizes = [16u32, 32, 48, 64, 128, 256];
    let pngs: Vec<Vec<u8>> = sizes
        .iter()
        .map(|&s| {
            let mut v = vec![];
            let img = image::RgbaImage::from_raw(s, s, rgba(s)).unwrap();
            img.write_to(&mut std::io::Cursor::new(&mut v), image::ImageFormat::Png).unwrap();
            v
        })
        .collect();
    let mut ico = vec![0, 0, 1, 0];
    ico.extend((sizes.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * sizes.len();
    for (s, p) in sizes.iter().zip(&pngs) {
        ico.extend([if *s >= 256 { 0 } else { *s as u8 }, if *s >= 256 { 0 } else { *s as u8 }, 0, 0, 1, 0, 32, 0]);
        ico.extend((p.len() as u32).to_le_bytes());
        ico.extend((offset as u32).to_le_bytes());
        offset += p.len();
    }
    for p in &pngs {
        ico.extend(p);
    }
    std::fs::write(dir.join("icon.ico"), ico).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_has_transparent_corners_and_a_painted_centre() {
        let p = rgba(64);
        assert_eq!(p.len(), 64 * 64 * 4);
        assert_eq!(p[3], 0, "corner is transparent");
        let mid = (32 * 64 + 32) * 4;
        assert_eq!(p[mid + 3], 255);
    }
}
