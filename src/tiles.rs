//! Cut an output into overlapping tiles and tell when one changed enough to be read again.

use crate::capture::Rgb;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tile {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
}

/// cols x rows tiles over w x h, each grown by `overlap` px into its neighbours.
pub fn grid(w: usize, h: usize, cols: u32, rows: u32, overlap: u32) -> Vec<Tile> {
    let (cols, rows, ov) = (cols.max(1) as usize, rows.max(1) as usize, overlap as usize);
    let mut out = vec![];
    for r in 0..rows {
        for c in 0..cols {
            let x0 = (w * c / cols).saturating_sub(if c > 0 { ov / 2 } else { 0 });
            let y0 = (h * r / rows).saturating_sub(if r > 0 { ov / 2 } else { 0 });
            let x1 = (w * (c + 1) / cols + if c + 1 < cols { ov / 2 } else { 0 }).min(w);
            let y1 = (h * (r + 1) / rows + if r + 1 < rows { ov / 2 } else { 0 }).min(h);
            out.push(Tile { x: x0, y: y0, w: x1 - x0, h: y1 - y0 });
        }
    }
    out
}

pub const BLOCK: usize = 16;

/// Mean brightness of every 16x16 block of the tile (sampled every other pixel and row).
pub fn signature(img: &Rgb, t: &Tile) -> Vec<u8> {
    let bw = t.w.div_ceil(BLOCK);
    let bh = t.h.div_ceil(BLOCK);
    let mut sum = vec![0u32; bw * bh];
    let mut n = vec![0u32; bw * bh];
    for yy in (0..t.h).step_by(2) {
        let row = (t.y + yy) * img.w;
        let b_row = (yy / BLOCK) * bw;
        for xx in (0..t.w).step_by(2) {
            let i = (row + t.x + xx) * 3;
            let p = &img.data[i..i + 3];
            let luma = (p[0] as u32 * 77 + p[1] as u32 * 150 + p[2] as u32 * 29) >> 8;
            let b = b_row + xx / BLOCK;
            sum[b] += luma;
            n[b] += 1;
        }
    }
    sum.iter().zip(&n).map(|(s, n)| if *n > 0 { (s / n) as u8 } else { 0 }).collect()
}

/// Any block moved by more than a hair (a typed character, a new chat line, a scrolled page).
pub fn differs(a: &[u8], b: &[u8]) -> bool {
    a.len() != b.len() || a.iter().zip(b).any(|(x, y)| x.abs_diff(*y) > 3)
}

/// Nothing that could be text: every block nearly the same brightness (a wallpaper gradient, a black area).
pub fn is_flat(sig: &[u8]) -> bool {
    let (lo, hi) = sig.iter().fold((255u8, 0u8), |(lo, hi), v| (lo.min(*v), hi.max(*v)));
    hi.saturating_sub(lo) < 6 || sig.windows(2).all(|w| w[0].abs_diff(w[1]) < 3)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_covers_and_overlaps() {
        let g = grid(2560, 1440, 2, 2, 48);
        assert_eq!(g.len(), 4);
        assert_eq!(g[0], Tile { x: 0, y: 0, w: 1304, h: 744 });
        assert_eq!(g[3], Tile { x: 1256, y: 696, w: 1304, h: 744 });
        assert!(g.iter().all(|t| t.w * t.h <= 1_003_520)); // the OCR model's full-detail size
    }

    #[test]
    fn signatures_see_small_changes() {
        let mut img = Rgb { w: 64, h: 32, data: vec![200; 64 * 32 * 3] };
        let t = Tile { x: 0, y: 0, w: 64, h: 32 };
        let a = signature(&img, &t);
        assert!(is_flat(&a));
        for y in 4..12 {
            for x in 20..26 {
                let i = (y * 64 + x) * 3;
                img.data[i..i + 3].fill(20); // one dark glyph
            }
        }
        let b = signature(&img, &t);
        assert!(differs(&a, &b));
        assert!(!is_flat(&b));
    }
}
