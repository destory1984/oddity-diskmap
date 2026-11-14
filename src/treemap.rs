//! Squarified treemap layout (Bruls, Huizing, van Wijk 2000).

use crate::tree::Tree;

/// Marker index for the "free space" block.
pub const FREE: u32 = u32::MAX - 1;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct R {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl R {
    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && py >= self.y && px < self.x + self.w && py < self.y + self.h
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Item {
    pub idx: u32,
    pub rect: R,
    pub depth: u16,
    /// Room for a title bar was reserved inside this rect.
    pub labeled: bool,
}

const HEADER: f32 = 18.0;
const PAD: f32 = 2.0;
const MIN_SIDE: f32 = 3.0;

fn worst(sum: f64, rmax: f64, rmin: f64, side: f64) -> f64 {
    let s2 = sum * sum;
    let w2 = side * side;
    (w2 * rmax / s2).max(s2 / (w2 * rmin))
}

/// Split `rect` among `items` (id, value), values sorted descending and > 0.
pub fn squarify(items: &[(u32, u64)], rect: R) -> Vec<(u32, R)> {
    let total: f64 = items.iter().map(|&(_, v)| v as f64).sum();
    let mut out = Vec::with_capacity(items.len());
    if total <= 0.0 || rect.w <= 0.0 || rect.h <= 0.0 {
        return out;
    }
    let scale = (rect.w as f64 * rect.h as f64) / total;
    let areas: Vec<f64> = items.iter().map(|&(_, v)| v as f64 * scale).collect();
    let (mut x, mut y, mut w, mut h) = (rect.x as f64, rect.y as f64, rect.w as f64, rect.h as f64);
    let mut i = 0;
    while i < items.len() {
        let side = w.min(h);
        let mut j = i;
        let mut sum = 0.0;
        let mut best = f64::INFINITY;
        while j < items.len() {
            let s = sum + areas[j];
            let wr = worst(s, areas[i], areas[j], side);
            if wr <= best {
                best = wr;
                sum = s;
                j += 1;
            } else {
                break;
            }
        }
        if w >= h {
            // column on the left
            let cw = if h > 0.0 { sum / h } else { 0.0 };
            let mut yy = y;
            for k in i..j {
                let ih = if cw > 0.0 { areas[k] / cw } else { 0.0 };
                out.push((items[k].0, R { x: x as f32, y: yy as f32, w: cw as f32, h: ih as f32 }));
                yy += ih;
            }
            x += cw;
            w -= cw;
        } else {
            // row on top
            let rh = if w > 0.0 { sum / w } else { 0.0 };
            let mut xx = x;
            for k in i..j {
                let iw = if rh > 0.0 { areas[k] / rh } else { 0.0 };
                out.push((items[k].0, R { x: xx as f32, y: y as f32, w: iw as f32, h: rh as f32 }));
                xx += iw;
            }
            y += rh;
            h -= rh;
        }
        i = j;
    }
    out
}

/// Lay out `root` (and optionally a free-space block beside it) into `rect`.
pub fn layout(tree: &Tree, root: u32, rect: R, use_alloc: bool, free: Option<u64>) -> Vec<Item> {
    let mut out = Vec::new();
    match free {
        Some(free) if free > 0 => {
            let mut top = vec![(root, tree.value(root, use_alloc).max(1)), (FREE, free)];
            top.sort_by(|a, b| b.1.cmp(&a.1));
            for (id, r) in squarify(&top, rect) {
                if id == FREE {
                    out.push(Item { idx: FREE, rect: r, depth: 0, labeled: true });
                } else {
                    place(tree, id, r, 0, use_alloc, &mut out);
                }
            }
        }
        _ => place(tree, root, rect, 0, use_alloc, &mut out),
    }
    out
}

fn place(tree: &Tree, idx: u32, r: R, depth: u16, use_alloc: bool, out: &mut Vec<Item>) {
    let node = &tree.nodes[idx as usize];
    let labeled = node.is_dir && r.w > 60.0 && r.h > HEADER + 12.0;
    out.push(Item { idx, rect: r, depth, labeled });
    if !node.is_dir || r.w < MIN_SIDE * 2.0 || r.h < MIN_SIDE * 2.0 {
        return;
    }
    let top = if labeled { HEADER } else { PAD };
    let inner = R { x: r.x + PAD, y: r.y + top, w: r.w - 2.0 * PAD, h: r.h - top - PAD };
    if inner.w < MIN_SIDE || inner.h < MIN_SIDE {
        return;
    }
    let mut kids: Vec<(u32, u64)> = node
        .children
        .iter()
        .map(|&c| (c, tree.value(c, use_alloc)))
        .filter(|&(_, v)| v > 0)
        .collect();
    if kids.is_empty() {
        return;
    }
    kids.sort_unstable_by(|a, b| b.1.cmp(&a.1));
    // Children far too small to see are merged into the parent's own fill.
    let total: u64 = kids.iter().map(|k| k.1).sum();
    let area = inner.w as f64 * inner.h as f64;
    let min_v = (total as f64 * (MIN_SIDE * MIN_SIDE) as f64 / area) as u64;
    let keep = kids.iter().take_while(|k| k.1 >= min_v).count();
    let rest: u64 = kids[keep..].iter().map(|k| k.1).sum();
    kids.truncate(keep);
    if rest > 0 {
        kids.push((u32::MAX, rest)); // invisible filler keeps proportions honest
    }
    for (c, cr) in squarify(&kids, inner) {
        if c == u32::MAX || cr.w < 1.0 || cr.h < 1.0 {
            continue;
        }
        place(tree, c, cr, depth + 1, use_alloc, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn areas_add_up() {
        let items = [(0, 6), (1, 6), (2, 4), (3, 3), (4, 2), (5, 2), (6, 1)];
        let rect = R { x: 0.0, y: 0.0, w: 6.0, h: 4.0 };
        let out = squarify(&items, rect);
        assert_eq!(out.len(), 7);
        for ((_, v), (_, r)) in items.iter().zip(&out) {
            assert!((r.w * r.h - *v as f32).abs() < 1e-3, "{r:?} vs {v}");
            assert!(r.x >= -1e-3 && r.y >= -1e-3 && r.x + r.w <= 6.001 && r.y + r.h <= 4.001);
        }
    }
}
