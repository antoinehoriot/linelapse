//! Ordered squarified treemap: items keep their input order, so a directory stays in the
//! same place from frame to frame while its area changes. Sorting by size each frame
//! would make the map jump around while scrubbing.

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Lay out `weights` (in order) inside `r`. Zero weights get an empty rect.
pub fn layout(weights: &[f64], r: Rect) -> Vec<Rect> {
    let mut out = vec![
        Rect {
            x: r.x,
            y: r.y,
            w: 0.0,
            h: 0.0
        };
        weights.len()
    ];
    let total: f64 = weights.iter().sum();
    if total <= 0.0 || r.w <= 0.0 || r.h <= 0.0 {
        return out;
    }
    let scale = (r.w as f64 * r.h as f64) / total;
    let idx: Vec<usize> = (0..weights.len()).filter(|&i| weights[i] > 0.0).collect();
    let mut rest = r;
    let mut start = 0;
    while start < idx.len() {
        let side = rest.w.min(rest.h) as f64;
        // Grow the row while the worst aspect ratio improves.
        let mut end = start + 1;
        let mut row_sum = weights[idx[start]] * scale;
        let mut best = worst(&idx[start..end], weights, scale, row_sum, side);
        while end < idx.len() {
            let s2 = row_sum + weights[idx[end]] * scale;
            let w2 = worst(&idx[start..end + 1], weights, scale, s2, side);
            if w2 > best {
                break;
            }
            best = w2;
            row_sum = s2;
            end += 1;
        }
        if end == idx.len() {
            // Last row takes all remaining space, which absorbs float error.
            row_sum = rest.w as f64 * rest.h as f64;
        }
        let horizontal = rest.w >= rest.h; // row is a column on the left when wider than tall
        let thick = (row_sum / side) as f32;
        let row_w: f64 = idx[start..end].iter().map(|&i| weights[i] * scale).sum();
        let mut off = 0.0f32;
        for &i in &idx[start..end] {
            let len = ((weights[i] * scale / row_w) * side) as f32;
            out[i] = if horizontal {
                Rect {
                    x: rest.x,
                    y: rest.y + off,
                    w: thick,
                    h: len,
                }
            } else {
                Rect {
                    x: rest.x + off,
                    y: rest.y,
                    w: len,
                    h: thick,
                }
            };
            off += len;
        }
        if horizontal {
            rest.x += thick;
            rest.w = (rest.w - thick).max(0.0);
        } else {
            rest.y += thick;
            rest.h = (rest.h - thick).max(0.0);
        }
        start = end;
    }
    out
}

fn worst(row: &[usize], w: &[f64], scale: f64, sum: f64, side: f64) -> f64 {
    let (mut mx, mut mn) = (0.0f64, f64::MAX);
    for &i in row {
        let a = w[i] * scale;
        mx = mx.max(a);
        mn = mn.min(a);
    }
    let (s2, side2) = (sum * sum, side * side);
    (side2 * mx / s2).max(s2 / (side2 * mn))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn areas_proportional_inside_and_disjoint() {
        let w = [50.0, 0.0, 30.0, 10.0, 5.0, 3.0, 1.0, 1.0, 0.5, 20.0];
        let r = Rect {
            x: 10.0,
            y: 20.0,
            w: 800.0,
            h: 500.0,
        };
        let out = layout(&w, r);
        let total: f64 = w.iter().sum();
        for (i, q) in out.iter().enumerate() {
            let want = w[i] / total * (800.0 * 500.0);
            let got = (q.w * q.h) as f64;
            assert!(
                (got - want).abs() < want * 0.02 + 1.0,
                "item {i}: {got} vs {want}"
            );
            if w[i] > 0.0 {
                assert!(q.x >= r.x - 0.01 && q.y >= r.y - 0.01);
                assert!(q.x + q.w <= r.x + r.w + 0.01 && q.y + q.h <= r.y + r.h + 0.01);
            }
        }
        for i in 0..out.len() {
            for j in i + 1..out.len() {
                let (a, b) = (out[i], out[j]);
                let ox = (a.x + a.w).min(b.x + b.w) - a.x.max(b.x);
                let oy = (a.y + a.h).min(b.y + b.h) - a.y.max(b.y);
                assert!(ox <= 0.01 || oy <= 0.01, "{i} overlaps {j}");
            }
        }
    }

    #[test]
    fn empty_and_zero_inputs() {
        let r = Rect {
            x: 0.0,
            y: 0.0,
            w: 10.0,
            h: 10.0,
        };
        assert!(layout(&[], r).is_empty());
        assert!(layout(&[0.0, 0.0], r).iter().all(|q| q.w == 0.0));
    }
}
