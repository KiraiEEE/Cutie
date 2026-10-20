use std::io::{self, Write};
use std::path::Path;

const SIZES: [usize; 4] = [16, 32, 48, 256];

pub fn write_ico_file(path: &Path) -> io::Result<()> {
    let mut out = Vec::new();
    write_ico(&mut out)?;
    let mut f = std::fs::File::create(path)?;
    f.write_all(&out)
}

pub fn write_ico(out: &mut Vec<u8>) -> io::Result<()> {
    let images: Vec<(u16, Vec<u8>)> = SIZES
        .iter()
        .map(|&s| (s as u16, render_32bpp_bgra(s)))
        .collect();

    let count = images.len() as u16;
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());

    let mut offset = 6 + 16 * images.len() as u32;
    let mut entries = Vec::new();
    for (size, data) in &images {
        let dim = if *size >= 256 { 0u8 } else { *size as u8 };
        entries.push((dim, data.clone(), offset));
        offset += data.len() as u32;
    }

    for (dim, data, off) in &entries {
        out.push(*dim);
        out.push(*dim);
        out.push(0);
        out.push(0);
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&32u16.to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&off.to_le_bytes());
    }

    for (_, data, _) in &entries {
        out.extend_from_slice(data);
    }
    Ok(())
}

fn render_32bpp_bgra(size: usize) -> Vec<u8> {
    let ss = if size <= 32 { 3 } else { 4 };
    let mut bgra = vec![0u8; size * size * 4];
    for y in 0..size {
        for x in 0..size {
            let mut r = 0.0;
            let mut g = 0.0;
            let mut b = 0.0;
            let mut a = 0.0;
            for sy in 0..ss {
                for sx in 0..ss {
                    let u = (x as f32 + (sx as f32 + 0.5) / ss as f32) / size as f32;
                    let v = (y as f32 + (sy as f32 + 0.5) / ss as f32) / size as f32;
                    let (pr, pg, pb, pa) = sample(u, v);
                    r += pr;
                    g += pg;
                    b += pb;
                    a += pa;
                }
            }
            let n = (ss * ss) as f32;
            let i = (y * size + x) * 4;
            bgra[i] = (b / n * 255.0) as u8;
            bgra[i + 1] = (g / n * 255.0) as u8;
            bgra[i + 2] = (r / n * 255.0) as u8;
            bgra[i + 3] = (a / n * 255.0) as u8;
        }
    }

    // pack into BMP-in-ICO
    let header_size = 40u32;
    let xor_size = (size * size * 4) as u32;
    let mask_row = size.div_ceil(32) * 4;
    let and_size = (mask_row * size) as u32;
    let total = (header_size + xor_size + and_size) as usize;
    let mut out = vec![0u8; total];

    out[0..4].copy_from_slice(&40u32.to_le_bytes());
    out[4..8].copy_from_slice(&(size as i32).to_le_bytes());
    out[8..12].copy_from_slice(&((size * 2) as i32).to_le_bytes());
    out[12..14].copy_from_slice(&1u16.to_le_bytes());
    out[14..16].copy_from_slice(&32u16.to_le_bytes());
    out[20..24].copy_from_slice(&xor_size.to_le_bytes());

    let xor_off = header_size as usize;
    for y in 0..size {
        let src_row = (size - 1 - y) * size * 4;
        let dst_row = xor_off + y * size * 4;
        out[dst_row..dst_row + size * 4].copy_from_slice(&bgra[src_row..src_row + size * 4]);
    }

    let and_off = xor_off + xor_size as usize;
    for y in 0..size {
        let row = and_off + y * mask_row;
        for x in 0..size {
            let alpha = bgra[(y * size + x) * 4 + 3];
            if alpha < 128 {
                out[row + x / 8] |= 0x80 >> (x % 8);
            }
        }
    }
    out
}

fn sample(u: f32, v: f32) -> (f32, f32, f32, f32) {
    let px = u * 2.0 - 1.0;
    let py = v * 2.0 - 1.0;

    // rounded square background
    let d = sd_round_rect(px, py, 0.88, 0.88, 0.22);
    let mask = smoothstep(0.025, -0.025, d);
    if mask <= 0.0 {
        return (0.0, 0.0, 0.0, 0.0);
    }

    // gradient pink → purple
    let t = (v * 0.7 + u * 0.3).clamp(0.0, 1.0);
    let mut r = lerp(1.0, 0.55, t);
    let mut g = lerp(0.44, 0.36, t);
    let mut b = lerp(0.70, 0.96, t);

    // subtle top gloss
    let gloss = smoothstep(0.6, -0.2, py + 0.5) * 0.18;
    r = (r + gloss).min(1.0);
    g = (g + gloss).min(1.0);
    b = (b + gloss).min(1.0);

    // white download arrow
    let arrow = sd_arrow(px, py);
    let am = smoothstep(0.02, -0.02, arrow);
    r = lerp(r, 1.0, am);
    g = lerp(g, 1.0, am);
    b = lerp(b, 1.0, am);

    (r, g, b, mask)
}

fn sd_round_rect(px: f32, py: f32, hw: f32, hh: f32, r: f32) -> f32 {
    let qx = (px.abs() - (hw - r)).max(0.0);
    let qy = (py.abs() - (hh - r)).max(0.0);
    (qx * qx + qy * qy).sqrt() + (px.abs().max(hw) - hw).min(0.0) + (py.abs().max(hh) - hh) - r
}

/// Simple, reliable down-arrow: vertical bar + triangle head.
fn sd_arrow(px: f32, py: f32) -> f32 {
    let shaft = sd_round_rect(px, py + 0.12, 0.10, 0.26, 0.08);
    let head = sd_triangle((px, py), [(0.0, 0.58), (-0.36, 0.06), (0.36, 0.06)]);
    shaft.min(head)
}

type Pt = (f32, f32);

/// Signed distance to a triangle; negative inside.
fn sd_triangle(p: Pt, tri: [Pt; 3]) -> f32 {
    let [(x0, y0), (x1, y1), (x2, y2)] = tri;
    let d1 = sign(p, (x0, y0), (x1, y1));
    let d2 = sign(p, (x1, y1), (x2, y2));
    let d3 = sign(p, (x2, y2), (x0, y0));
    let neg = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
    let pos = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;
    let e0 = edge_dist(p, (x0, y0), (x1, y1));
    let e1 = edge_dist(p, (x1, y1), (x2, y2));
    let e2 = edge_dist(p, (x2, y2), (x0, y0));
    let nearest = e0.min(e1).min(e2);
    if neg && pos {
        nearest
    } else {
        -nearest
    }
}

fn sign(p: Pt, a: Pt, b: Pt) -> f32 {
    (p.0 - b.0) * (a.1 - b.1) - (a.0 - b.0) * (p.1 - b.1)
}

fn edge_dist(p: Pt, a: Pt, b: Pt) -> f32 {
    let (ex, ey) = (b.0 - a.0, b.1 - a.1);
    let len2 = (ex * ex + ey * ey).max(1e-8);
    let t = (((p.0 - a.0) * ex + (p.1 - a.1) * ey) / len2).clamp(0.0, 1.0);
    let (dx, dy) = (p.0 - (a.0 + ex * t), p.1 - (a.1 + ey * t));
    (dx * dx + dy * dy).sqrt().max(0.0)
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

fn smoothstep(a: f32, b: f32, x: f32) -> f32 {
    let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}
