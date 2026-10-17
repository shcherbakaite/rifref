//! Stroke smoothing.
//!
//! Mouse samples arrive sparsely on fast strokes, so a polyline through them looks angular.
//!
//! - `densify`: a centripetal Catmull-Rom spline through every sample fills the gaps. Used for
//!   the live preview, and for finished strokes when smoothing is off.
//! - `finish_stroke` with smoothing on: the raw samples are split at sharp corners and each piece
//!   is fitted with cubic Béziers (Schneider, "An Algorithm for Automatically Fitting Digitized
//!   Curves", Graphics Gems 1990). That bridges sparse samples with smooth curves and also
//!   removes hand jitter; the curves are then resampled densely for drawing.

use eframe::egui::{Pos2, Vec2};

pub type Cubic = [Pos2; 4];

/// Corners: direction changes sharper than this (cosine of the turn angle) are kept sharp.
const CORNER_COS: f32 = 0.35; // ≈70°
/// Arc length (screen pixels) used to judge direction for corners and end tangents.
const WINDOW_PX: f32 = 10.0;

/// Spacing of generated points, in screen pixels.
const STEP_PX: f32 = 1.5;

/// Fill the gaps between mouse samples with a smooth curve through them.
/// `px` is one screen pixel in world units.
pub fn densify(points: &[Pos2], px: f32) -> Vec<Pos2> {
    fill_gaps(points, px, 0.0)
}

/// Spline through the samples, inserting points only where samples are more than `min_gap_px`
/// screen pixels apart.
fn fill_gaps(points: &[Pos2], px: f32, min_gap_px: f32) -> Vec<Pos2> {
    let pts = dedup(points, 0.5 * px);
    let n = pts.len();
    if n < 3 {
        return pts;
    }
    let step = STEP_PX * px;
    let min_gap = min_gap_px * px;
    // Phantom points beyond the ends, mirrored, so the end segments curve naturally.
    let at = |i: isize| -> Pos2 {
        if i < 0 {
            pts[0] + (pts[0] - pts[1])
        } else if i as usize >= n {
            pts[n - 1] + (pts[n - 1] - pts[n - 2])
        } else {
            pts[i as usize]
        }
    };
    let mut out = Vec::with_capacity(n * 2);
    out.push(pts[0]);
    for i in 0..n as isize - 1 {
        let (p0, p1, p2, p3) = (at(i - 1), at(i), at(i + 1), at(i + 2));
        if p1.distance(p2) <= min_gap {
            out.push(p2);
            continue;
        }
        // The curve is a bit longer than the chord; oversample slightly.
        let steps = ((1.25 * p1.distance(p2) / step).ceil() as usize).clamp(1, 400);
        for k in 1..=steps {
            out.push(centripetal_catmull_rom(p0, p1, p2, p3, k as f32 / steps as f32));
        }
    }
    out
}

/// Point at `t` (0..=1) between `p1` and `p2` on a centripetal Catmull-Rom spline
/// (Barry–Goldman pyramid). Centripetal knots avoid overshoot and self-intersections.
fn centripetal_catmull_rom(p0: Pos2, p1: Pos2, p2: Pos2, p3: Pos2, t: f32) -> Pos2 {
    let knot = |a: Pos2, b: Pos2| a.distance(b).sqrt().max(1e-4);
    let t0 = 0.0;
    let t1 = t0 + knot(p0, p1);
    let t2 = t1 + knot(p1, p2);
    let t3 = t2 + knot(p2, p3);
    let tt = t1 + (t2 - t1) * t;
    let lerp = |a: Vec2, b: Vec2, ta: f32, tb: f32| a * ((tb - tt) / (tb - ta)) + b * ((tt - ta) / (tb - ta));
    let (p0, p1, p2, p3) = (p0.to_vec2(), p1.to_vec2(), p2.to_vec2(), p3.to_vec2());
    let a1 = lerp(p0, p1, t0, t1);
    let a2 = lerp(p1, p2, t1, t2);
    let a3 = lerp(p2, p3, t2, t3);
    let b1 = lerp(a1, a2, t0, t2);
    let b2 = lerp(a2, a3, t1, t3);
    lerp(b1, b2, t1, t2).to_pos2()
}

/// Smooth a finished stroke. With `strength` 0 the spline passes through every sample; above
/// that (range 0..=1) curves are fitted within a tolerance that grows with strength. `px` is one
/// screen pixel in world units, so tolerances match what the user saw while drawing.
pub fn finish_stroke(points: &[Pos2], strength: f32, px: f32) -> Vec<Pos2> {
    if strength <= 0.0 {
        return densify(points, px);
    }
    // Give the fit a reference curve across long gaps between sparse samples, but leave dense
    // stretches raw: splining jittery input would turn the jitter into tiny loops.
    let pts = fill_gaps(points, px, 4.0);
    if pts.len() < 3 {
        return densify(&pts, px);
    }
    // The fit's max-error test is sensitive to single jittery samples; soften them first.
    let pts = soften(&pts, 4.0 * strength * px);
    let max_error = (0.5 + 3.5 * strength) * px;
    let window = WINDOW_PX * px;
    let mut curves = Vec::new();
    let cuts = corners(&pts, window);
    for w in cuts.windows(2) {
        let seg = &pts[w[0]..=w[1]];
        let t1 = end_tangent(seg, window, false);
        let t2 = end_tangent(seg, window, true);
        fit_cubic(seg, t1, t2, max_error, window, &mut curves);
    }
    sample(&curves, STEP_PX * px)
}

fn dedup(points: &[Pos2], min_dist: f32) -> Vec<Pos2> {
    let mut out: Vec<Pos2> = Vec::with_capacity(points.len());
    for (i, p) in points.iter().enumerate() {
        let last = i == points.len() - 1;
        match out.last() {
            Some(q) if q.distance(*p) < min_dist && !last => {}
            Some(q) if q.distance(*p) < min_dist && out.len() > 1 => {
                *out.last_mut().unwrap() = *p;
            }
            _ => out.push(*p),
        }
    }
    out
}

/// Gaussian-weighted average of each point with neighbours within `radius` (straight-line
/// distance), keeping the endpoints fixed. Removes high-frequency jitter.
fn soften(pts: &[Pos2], radius: f32) -> Vec<Pos2> {
    let n = pts.len();
    if radius <= 0.0 || n < 3 {
        return pts.to_vec();
    }
    let inv = 1.0 / (2.0 * (radius * 0.5).powi(2));
    let mut out = Vec::with_capacity(n);
    out.push(pts[0]);
    for i in 1..n - 1 {
        let (mut sum, mut wsum) = (Vec2::ZERO, 0.0);
        let mut add = |j: usize| -> bool {
            let d2 = pts[j].distance_sq(pts[i]);
            if d2 > radius * radius {
                return false;
            }
            let w = (-d2 * inv).exp();
            sum += pts[j].to_vec2() * w;
            wsum += w;
            true
        };
        add(i);
        for j in (0..i).rev() {
            if !add(j) {
                break;
            }
        }
        for j in i + 1..n {
            if !add(j) {
                break;
            }
        }
        out.push((sum / wsum).to_pos2());
    }
    out.push(pts[n - 1]);
    out
}

fn arc_lengths(pts: &[Pos2]) -> Vec<f32> {
    let mut s = Vec::with_capacity(pts.len());
    let mut acc = 0.0;
    for (i, p) in pts.iter().enumerate() {
        if i > 0 {
            acc += pts[i - 1].distance(*p);
        }
        s.push(acc);
    }
    s
}

/// Indices to split at: the ends plus the sharpest point of every sharp turn. Direction is
/// judged between points `window` away in straight-line distance, so jitter is ignored.
fn corners(pts: &[Pos2], window: f32) -> Vec<usize> {
    let n = pts.len();
    let mut cuts = vec![0];
    let mut best: Option<(usize, f32)> = None;
    for i in 1..n - 1 {
        let before = (0..i).rev().find(|&j| pts[j].distance(pts[i]) >= window);
        let after = (i + 1..n).find(|&k| pts[k].distance(pts[i]) >= window);
        let turn = before.zip(after).map(|(j, k)| {
            let a = (pts[i] - pts[j]).normalized();
            let b = (pts[k] - pts[i]).normalized();
            a.dot(b)
        });
        match turn {
            Some(c) if c < CORNER_COS => {
                if best.is_none_or(|(_, bc)| c < bc) {
                    best = Some((i, c));
                }
            }
            _ => {
                if let Some((bi, _)) = best.take() {
                    cuts.push(bi);
                }
            }
        }
    }
    if let Some((bi, _)) = best {
        cuts.push(bi);
    }
    cuts.push(n - 1);
    cuts.dedup();
    cuts
}

/// Unit tangent at an end of `seg`, pointing into the curve, judged over a short arc.
fn end_tangent(seg: &[Pos2], window: f32, at_end: bool) -> Vec2 {
    let total: f32 = seg.windows(2).map(|w| w[0].distance(w[1])).sum();
    let reach = window.min(total / 3.0);
    let it: Box<dyn Iterator<Item = &Pos2>> = if at_end {
        Box::new(seg.iter().rev())
    } else {
        Box::new(seg.iter())
    };
    let pts: Vec<&Pos2> = it.collect();
    let origin = *pts[0];
    let target = pts
        .iter()
        .skip(1)
        .find(|p| p.distance(origin) >= reach)
        .unwrap_or(&pts[pts.len() - 1]);
    let t = **target - origin;
    if t.length_sq() > 0.0 {
        t.normalized()
    } else {
        Vec2::X
    }
}

fn bezier(c: &Cubic, t: f32) -> Pos2 {
    let mt = 1.0 - t;
    let v = c[0].to_vec2() * (mt * mt * mt)
        + c[1].to_vec2() * (3.0 * mt * mt * t)
        + c[2].to_vec2() * (3.0 * mt * t * t)
        + c[3].to_vec2() * (t * t * t);
    v.to_pos2()
}

fn bezier_d1(c: &Cubic, t: f32) -> Vec2 {
    let mt = 1.0 - t;
    (c[1] - c[0]) * (3.0 * mt * mt) + (c[2] - c[1]) * (6.0 * mt * t) + (c[3] - c[2]) * (3.0 * t * t)
}

fn bezier_d2(c: &Cubic, t: f32) -> Vec2 {
    let mt = 1.0 - t;
    ((c[2] - c[1]) - (c[1] - c[0])) * (6.0 * mt) + ((c[3] - c[2]) - (c[2] - c[1])) * (6.0 * t)
}

fn chord_params(d: &[Pos2]) -> Vec<f32> {
    let s = arc_lengths(d);
    let total = *s.last().unwrap();
    s.iter()
        .map(|v| if total > 0.0 { v / total } else { 0.0 })
        .collect()
}

/// Least-squares cubic with fixed end tangent directions.
fn generate(d: &[Pos2], u: &[f32], t1: Vec2, t2: Vec2) -> Cubic {
    let (first, last) = (d[0], d[d.len() - 1]);
    let (mut c00, mut c01, mut c11, mut x0, mut x1) = (0.0, 0.0, 0.0, 0.0, 0.0);
    for (p, &t) in d.iter().zip(u) {
        let mt = 1.0 - t;
        let (b0, b1, b2, b3) = (mt * mt * mt, 3.0 * mt * mt * t, 3.0 * mt * t * t, t * t * t);
        let a0 = t1 * b1;
        let a1 = t2 * b2;
        c00 += a0.dot(a0);
        c01 += a0.dot(a1);
        c11 += a1.dot(a1);
        let tmp = *p - (first.to_vec2() * (b0 + b1) + last.to_vec2() * (b2 + b3)).to_pos2();
        x0 += a0.dot(tmp);
        x1 += a1.dot(tmp);
    }
    let det = c00 * c11 - c01 * c01;
    let seg_len = first.distance(last);
    let eps = 1e-6 * seg_len;
    let (mut al, mut ar) = if det.abs() > 1e-12 {
        ((x0 * c11 - x1 * c01) / det, (c00 * x1 - c01 * x0) / det)
    } else {
        (0.0, 0.0)
    };
    // Degenerate fits (handles too short, or wildly long where noisy points double back):
    // fall back to the Wu/Barsky heuristic and let the error check split the segment.
    if !(al.is_finite() && ar.is_finite())
        || al < eps
        || ar < eps
        || al > 2.0 * seg_len
        || ar > 2.0 * seg_len
    {
        al = seg_len / 3.0;
        ar = seg_len / 3.0;
    }
    [first, first + t1 * al, last + t2 * ar, last]
}

fn max_error(d: &[Pos2], c: &Cubic, u: &[f32]) -> (f32, usize) {
    let mut worst = (0.0, d.len() / 2);
    for i in 1..d.len() - 1 {
        let e = bezier(c, u[i]).distance(d[i]);
        if e > worst.0 {
            worst = (e, i);
        }
    }
    worst
}

/// One Newton–Raphson step per sample towards the closest point on the curve.
fn reparameterize(d: &[Pos2], u: &[f32], c: &Cubic) -> Vec<f32> {
    d.iter()
        .zip(u)
        .map(|(p, &t)| {
            let diff = bezier(c, t) - *p;
            let d1 = bezier_d1(c, t);
            let d2 = bezier_d2(c, t);
            let den = d1.dot(d1) + diff.dot(d2);
            if den.abs() < 1e-12 {
                t
            } else {
                (t - diff.dot(d1) / den).clamp(0.0, 1.0)
            }
        })
        .collect()
}

fn fit_cubic(d: &[Pos2], t1: Vec2, t2: Vec2, err: f32, window: f32, out: &mut Vec<Cubic>) {
    if d.len() == 2 {
        let dist = d[0].distance(d[1]) / 3.0;
        out.push([d[0], d[0] + t1 * dist, d[1] + t2 * dist, d[1]]);
        return;
    }
    let mut u = chord_params(d);
    let mut c = generate(d, &u, t1, t2);
    let (mut e, mut split) = max_error(d, &c, &u);
    if e < err {
        out.push(c);
        return;
    }
    // Close enough that a better parameterization may fix it.
    if e < err * 4.0 {
        for _ in 0..12 {
            u = reparameterize(d, &u, &c);
            c = generate(d, &u, t1, t2);
            (e, split) = max_error(d, &c, &u);
            if e < err {
                out.push(c);
                return;
            }
        }
    }
    let split = split.clamp(1, d.len() - 2);
    let tc = center_tangent(d, split, window);
    fit_cubic(&d[..=split], t1, tc, err, window, out);
    fit_cubic(&d[split..], -tc, t2, err, window, out);
}

/// Tangent at an interior split point (pointing backwards), judged over a short arc.
fn center_tangent(d: &[Pos2], i: usize, window: f32) -> Vec2 {
    let reach = window * 0.5;
    let before = d[..i]
        .iter()
        .rev()
        .find(|p| p.distance(d[i]) >= reach)
        .unwrap_or(&d[0]);
    let after = d[i + 1..]
        .iter()
        .find(|p| p.distance(d[i]) >= reach)
        .unwrap_or(&d[d.len() - 1]);
    let t = *before - *after;
    if t.length_sq() > 0.0 {
        t.normalized()
    } else {
        (d[i - 1] - d[i + 1]).normalized()
    }
}

/// Sample curves roughly every `step` world units.
fn sample(curves: &[Cubic], step: f32) -> Vec<Pos2> {
    let mut out = Vec::new();
    for (k, c) in curves.iter().enumerate() {
        let len = c[0].distance(c[1]) + c[1].distance(c[2]) + c[2].distance(c[3]);
        let n = ((len / step).ceil() as usize).clamp(1, 400);
        let start = if k == 0 { 0 } else { 1 };
        for i in start..=n {
            out.push(bezier(c, i as f32 / n as f32));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::pos2;

    fn noise(seed: &mut u32) -> f32 {
        *seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        ((*seed >> 16) % 1000) as f32 / 1000.0 - 0.5
    }

    #[test]
    fn densify_passes_through_samples() {
        let pts = vec![pos2(0.0, 0.0), pos2(40.0, 30.0), pos2(80.0, -10.0), pos2(120.0, 20.0)];
        let out = densify(&pts, 1.0);
        for p in &pts {
            let closest = out.iter().map(|q| q.distance(*p)).fold(f32::INFINITY, f32::min);
            assert!(closest < 1e-3, "sample {p:?} missed by {closest}");
        }
        // Gaps are filled: no step longer than ~1.5px.
        let longest = out.windows(2).map(|w| w[0].distance(w[1])).fold(0.0, f32::max);
        assert!(longest <= 2.0, "longest step {longest}");
    }

    #[test]
    fn sparse_circle_becomes_round() {
        // A fast circle: only 10 mouse samples on radius 100 (~63px apart).
        let pts: Vec<Pos2> = (0..=10)
            .map(|i| {
                let a = i as f32 / 10.0 * std::f32::consts::TAU;
                pos2(100.0 * a.cos(), 100.0 * a.sin())
            })
            .collect();
        let radial = |ps: &[Pos2]| {
            ps.iter()
                .map(|p| (p.to_vec2().length() - 100.0).abs())
                .fold(0.0, f32::max)
        };
        // Straight segments sag ~4.9px inside the circle at their midpoints...
        let polyline: Vec<Pos2> = pts
            .windows(2)
            .flat_map(|w| (0..10).map(move |k| w[0] + (w[1] - w[0]) * (k as f32 / 10.0)))
            .collect();
        assert!(radial(&polyline) > 4.5);
        // ...while the spline stays within ~1px of the true circle (away from the open ends).
        let out = densify(&pts, 1.0);
        let middle = &out[out.len() / 10..out.len() * 9 / 10];
        assert!(radial(middle) < 1.0, "spline radial error {}", radial(middle));
    }

    /// Max deviation from a circle of radius 100, and the sharpest turn between samples.
    fn circle_quality(pts: &[Pos2]) -> (f32, f32) {
        let worst = pts
            .iter()
            .map(|p| (p.to_vec2().length() - 100.0).abs())
            .fold(0.0, f32::max);
        let sharpest = pts
            .windows(3)
            .filter(|w| w[0].distance(w[1]) > 0.5 && w[1].distance(w[2]) > 0.5)
            .map(|w| (w[1] - w[0]).normalized().dot((w[2] - w[1]).normalized()))
            .fold(1.0, f32::min);
        (worst, sharpest)
    }

    fn noisy_arc(amplitude: f32) -> Vec<Pos2> {
        let mut seed = 7;
        (0..315)
            .map(|i| {
                let a = i as f32 / 100.0;
                let p = pos2(100.0 * a.cos(), 100.0 * a.sin());
                // Mouse positions are whole pixels, plus some hand tremor.
                pos2(
                    (p.x + amplitude * noise(&mut seed)).round(),
                    (p.y + amplitude * noise(&mut seed)).round(),
                )
            })
            .collect()
    }

    #[test]
    fn slow_mouse_stroke_comes_out_smooth() {
        // A slowly drawn arc: dense samples, whole-pixel positions, ±0.75px tremor.
        let pts = noisy_arc(1.5);
        let (raw_err, raw_turn) = circle_quality(&pts);
        assert!(raw_turn < 0.0, "input should be jagged");
        let out = finish_stroke(&pts, 0.5, 1.0);
        assert_eq!(out[0], pts[0]);
        assert_eq!(*out.last().unwrap(), *pts.last().unwrap());
        let (err, turn) = circle_quality(&out);
        assert!(err <= raw_err + 0.5, "deviation {err} vs input {raw_err}");
        assert!(turn > 0.98, "sharpest turn cos {turn}");
    }

    #[test]
    fn heavy_jitter_is_reduced() {
        // ±2px jitter at every 1px sample (worse than any real mouse): full strength must still
        // leave the stroke much less jagged than the input, and no further off.
        let pts = noisy_arc(4.0);
        let (raw_err, raw_turn) = circle_quality(&pts);
        let (err, turn) = circle_quality(&finish_stroke(&pts, 1.0, 1.0));
        assert!(err <= raw_err + 0.5, "deviation {err} vs input {raw_err}");
        assert!(turn > raw_turn + 0.5, "sharpest turn cos {turn} vs input {raw_turn}");
    }

    #[test]
    fn sparse_circle_fit_is_round() {
        let pts: Vec<Pos2> = (0..=10)
            .map(|i| {
                let a = i as f32 / 10.0 * std::f32::consts::TAU;
                pos2(100.0 * a.cos(), 100.0 * a.sin())
            })
            .collect();
        let out = finish_stroke(&pts, 0.5, 1.0);
        // The fit may deviate up to its tolerance (2.25px at 50%) from the spline through the
        // samples, which itself is within ~1px of the true circle.
        let worst = out
            .iter()
            .map(|p| (p.to_vec2().length() - 100.0).abs())
            .fold(0.0, f32::max);
        assert!(worst < 4.0, "max radial error {worst}");
        let longest = out.windows(2).map(|w| w[0].distance(w[1])).fold(0.0, f32::max);
        assert!(longest < 2.0, "gap {longest}");
        // No visible corners: consecutive samples turn by only a few degrees.
        let sharpest = out
            .windows(3)
            .filter(|w| w[0].distance(w[1]) > 0.5 && w[1].distance(w[2]) > 0.5)
            .map(|w| (w[1] - w[0]).normalized().dot((w[2] - w[1]).normalized()))
            .fold(1.0, f32::min);
        assert!(sharpest > 0.98, "sharpest turn cos {sharpest}");
    }

    #[test]
    fn keeps_sharp_corners() {
        // An "L": right 100px then down 100px, with mild noise.
        let mut seed = 3;
        let mut pts: Vec<Pos2> = (0..=100).map(|i| pos2(i as f32, noise(&mut seed))).collect();
        pts.extend((1..=100).map(|i| pos2(100.0 + noise(&mut seed), i as f32)));
        assert_eq!(corners(&pts, 10.0).len(), 3);
        let out = finish_stroke(&pts, 0.5, 1.0);
        let closest = out
            .iter()
            .map(|p| p.distance(pos2(100.0, 0.0)))
            .fold(f32::INFINITY, f32::min);
        assert!(closest < 1.5, "corner rounded off by {closest}");
    }

    #[test]
    fn straight_line_stays_straight() {
        let pts: Vec<Pos2> = (0..100).map(|i| pos2(i as f32 * 2.0, i as f32)).collect();
        let out = finish_stroke(&pts, 1.0, 1.0);
        for p in &out {
            assert!((p.y - p.x / 2.0).abs() < 0.01, "{p:?}");
        }
    }
}

