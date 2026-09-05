//! DXF input: parse bytes into `Part`s.
//!
//! Supported entities: LINE, ARC, CIRCLE, LWPOLYLINE (incl. bulges),
//! POLYLINE (2D), SPLINE (linearized), ELLIPSE (linearized). Anything else
//! produces a warning and is skipped. INSERT/blocks produce a warning
//! (Solid Edge flat exports are plain entities).
//!
//! Pipeline:
//! 1. Entities -> `Seg`s (arcs stay analytic; splines/ellipses are
//!    linearized into line segs at `curve_tol`).
//! 2. Chain segs into loops by endpoint proximity (CHAIN_TOL first, then a
//!    second pass with CHAIN_TOL_LOOSE for the leftovers). Unclosed chains
//!    -> warning, skipped.
//! 3. Containment tree via linearized rings: depth 0 = outer contour,
//!    depth 1 = its holes, depth 2 = a new part nested inside a hole (and
//!    so on, alternating). Multiple top-level loops in one file = multiple
//!    parts named "name", "name#2", ...
//! 4. Normalize: outer rings CCW, holes CW; translate each part so its
//!    outer bbox min corner sits at (0,0).
//! 5. If the DXF header says units are inches ($INSUNITS == 1), scale by
//!    25.4 with a warning; otherwise assume mm.

use crate::model::{
    CHAIN_TOL, CHAIN_TOL_LOOSE, Contour, Part, Pt, Seg, ring_bbox, ring_signed_area,
};
use dxf::Drawing;
use dxf::enums::Units;
use std::collections::BTreeMap;

pub struct ParsedFile {
    pub parts: Vec<Part>,
    pub warnings: Vec<String>,
}

/// Loops shorter than this (in |area|) are noise, not geometry.
const MIN_LOOP_AREA: f64 = 1e-6;
/// Two points closer than this are the same point.
const EPS: f64 = 1e-9;

/// Parse one DXF file. `name` is the source filename (used for part names),
/// `quantity` is copied onto every resulting part, `curve_tol` is the max
/// chord error (mm) for linearization (both for spline conversion and for
/// the cached `outer_poly`).
///
/// Errors only on unreadable files or files with zero usable closed loops;
/// recoverable oddities go into `warnings`.
pub fn parse_dxf(
    bytes: &[u8],
    name: &str,
    quantity: u32,
    curve_tol: f64,
) -> anyhow::Result<ParsedFile> {
    let mut warnings: Vec<String> = Vec::new();

    let mut reader = bytes;
    let drawing =
        Drawing::load(&mut reader).map_err(|e| anyhow::anyhow!("not a readable DXF file: {e}"))?;

    let curve_tol = if curve_tol.is_finite() && curve_tol > 1e-6 {
        curve_tol
    } else {
        0.25
    };

    // ---- units ---------------------------------------------------------
    let scale = unit_scale(drawing.header.default_drawing_units, &mut warnings);
    // Linearization happens in drawing units, so the chord tolerance has to
    // be expressed in drawing units too.
    let local_tol = (curve_tol / scale).max(1e-9);

    // ---- entities -> segments -----------------------------------------
    let mut segs: Vec<Seg> = Vec::new();
    let mut skipped: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut degenerate_arcs = 0usize;

    for e in drawing.entities() {
        match &e.specific {
            dxf::entities::EntityType::Line(l) => {
                let a = Pt::new(l.p1.x, l.p1.y);
                let b = Pt::new(l.p2.x, l.p2.y);
                if a.dist(&b) > EPS {
                    segs.push(Seg::Line { a, b });
                }
            }
            dxf::entities::EntityType::Arc(a) => match arc_seg(a) {
                Some(s) => segs.push(s),
                None => degenerate_arcs += 1,
            },
            dxf::entities::EntityType::Circle(c) => {
                if c.radius > EPS {
                    segs.push(Seg::Arc {
                        c: Pt::new(c.center.x, c.center.y),
                        r: c.radius,
                        start_deg: 0.0,
                        sweep_deg: 360.0,
                    });
                } else {
                    degenerate_arcs += 1;
                }
            }
            dxf::entities::EntityType::LwPolyline(p) => {
                let pts: Vec<(f64, f64, f64)> =
                    p.vertices.iter().map(|v| (v.x, v.y, v.bulge)).collect();
                emit_polyline(&pts, p.is_closed(), &mut segs);
            }
            dxf::entities::EntityType::Polyline(p) => {
                let pts: Vec<(f64, f64, f64)> = p
                    .vertices()
                    .map(|v| (v.location.x, v.location.y, v.bulge))
                    .collect();
                emit_polyline(&pts, p.is_closed(), &mut segs);
            }
            dxf::entities::EntityType::Spline(sp) => {
                emit_points(&spline_points(sp, local_tol), &mut segs);
            }
            dxf::entities::EntityType::Ellipse(el) => {
                emit_points(&ellipse_points(el, local_tol), &mut segs);
            }
            // SEQEND / VERTEX are structural children of POLYLINE and are
            // already folded into it by the reader - skipping them silently.
            dxf::entities::EntityType::Seqend(_) | dxf::entities::EntityType::Vertex(_) => {}
            other => {
                *skipped.entry(entity_kind(other)).or_insert(0) += 1;
            }
        }
    }

    if degenerate_arcs > 0 {
        warnings.push(format!("{degenerate_arcs} degenerate arc(s) skipped"));
    }
    for (kind, n) in &skipped {
        if *kind == "INSERT" {
            warnings.push(format!(
                "{n} INSERT entity/entities skipped (block references are not expanded)"
            ));
        } else {
            warnings.push(format!("{n} unsupported {kind} entity/entities skipped"));
        }
    }

    if (scale - 1.0).abs() > 1e-12 {
        for s in segs.iter_mut() {
            *s = scaled_seg(s, scale);
        }
    }

    // ---- chaining ------------------------------------------------------
    let (loops, open_chains) = chain_segments(segs);
    if open_chains > 0 {
        warnings.push(format!("{open_chains} open chains ignored"));
    }

    // ---- rings ---------------------------------------------------------
    let mut contours: Vec<Contour> = Vec::new();
    let mut rings: Vec<Vec<Pt>> = Vec::new();
    for l in loops {
        let c = Contour { segs: l };
        let ring = c.polyline(curve_tol);
        if ring.len() < 3 || ring_signed_area(&ring).abs() < MIN_LOOP_AREA {
            continue;
        }
        contours.push(c);
        rings.push(ring);
    }

    if contours.is_empty() {
        anyhow::bail!("no closed contours found");
    }

    // ---- containment tree ---------------------------------------------
    let n = contours.len();
    let areas: Vec<f64> = rings.iter().map(|r| ring_signed_area(r).abs()).collect();
    let boxes: Vec<(f64, f64, f64, f64)> = rings.iter().map(|r| ring_bbox(r)).collect();

    let mut depth = vec![0usize; n];
    let mut parent: Vec<Option<usize>> = vec![None; n];
    for i in 0..n {
        let mut best: Option<usize> = None;
        for j in 0..n {
            if i == j || areas[j] <= areas[i] {
                continue;
            }
            if !bbox_contains(&boxes[j], &boxes[i]) {
                continue;
            }
            if !point_in_ring(rings[i][0], &rings[j]) {
                continue;
            }
            depth[i] += 1;
            best = match best {
                Some(b) if areas[b] <= areas[j] => Some(b),
                _ => Some(j),
            };
        }
        parent[i] = best;
    }

    // ---- assemble parts -------------------------------------------------
    // Even depth = an outer contour (0 = top level, 2 = nested inside a
    // hole, ...). Odd depth = a hole of its immediate parent.
    let mut slot_of_ring: Vec<Option<usize>> = vec![None; n];
    let mut outer_ring_of_part: Vec<usize> = Vec::new();
    for (i, slot) in slot_of_ring.iter_mut().enumerate() {
        if depth[i].is_multiple_of(2) {
            *slot = Some(outer_ring_of_part.len());
            outer_ring_of_part.push(i);
        }
    }

    let mut hole_rings: Vec<Vec<usize>> = vec![Vec::new(); outer_ring_of_part.len()];
    for i in 0..n {
        if !depth[i].is_multiple_of(2)
            && let Some(p) = parent[i]
            && let Some(slot) = slot_of_ring[p]
        {
            hole_rings[slot].push(i);
        }
    }

    let mut parts: Vec<Part> = Vec::new();
    for (slot, &oi) in outer_ring_of_part.iter().enumerate() {
        let holes: Vec<Contour> = hole_rings[slot]
            .iter()
            .map(|&hi| contours[hi].clone())
            .collect();
        let part_name = if slot == 0 {
            name.to_string()
        } else {
            format!("{name}#{}", slot + 1)
        };
        // Orientation, origin normalization and areas happen in
        // Part::from_contours; the ring was already area-checked above.
        parts.push(Part::from_contours(
            part_name,
            quantity,
            contours[oi].clone(),
            holes,
            curve_tol,
        )?);
    }

    Ok(ParsedFile { parts, warnings })
}

/// Chain loose segments into closed loops. Exposed for tests.
/// Returns (closed loops, leftover open chains count).
pub fn chain_segments(segs: Vec<Seg>) -> (Vec<Vec<Seg>>, usize) {
    // 0 = free, 1 = consumed by a closed loop, 2 = part of a failed chain
    // in the current pass (released before the next, looser, pass).
    let mut state = vec![0u8; segs.len()];
    let mut loops: Vec<Vec<Seg>> = Vec::new();

    let (mut found, _) = chain_pass(&segs, &mut state, CHAIN_TOL);
    loops.append(&mut found);

    for s in state.iter_mut() {
        if *s == 2 {
            *s = 0;
        }
    }

    let (mut found, open) = chain_pass(&segs, &mut state, CHAIN_TOL_LOOSE);
    loops.append(&mut found);

    (loops, open)
}

/// One greedy chaining pass at `tol`. Segments belonging to a chain that
/// failed to close are marked `2` (retryable) rather than consumed.
fn chain_pass(segs: &[Seg], state: &mut [u8], tol: f64) -> (Vec<Vec<Seg>>, usize) {
    let mut loops: Vec<Vec<Seg>> = Vec::new();
    let mut open = 0usize;

    while let Some(start) = state.iter().position(|&s| s == 0) {
        let mut taken = vec![start];
        state[start] = 1;
        let mut chain: Vec<Seg> = vec![segs[start].clone()];

        // extend forward from the chain's end
        loop {
            if chain_closed(&chain, tol) {
                break;
            }
            let end = chain.last().unwrap().end();
            match nearest_free(segs, state, end, tol, true) {
                Some((i, rev)) => {
                    state[i] = 1;
                    taken.push(i);
                    chain.push(if rev {
                        segs[i].reversed()
                    } else {
                        segs[i].clone()
                    });
                }
                None => break,
            }
        }

        // extend backward from the chain's start
        while !chain_closed(&chain, tol) {
            let head = chain[0].start();
            match nearest_free(segs, state, head, tol, false) {
                Some((i, rev)) => {
                    state[i] = 1;
                    taken.push(i);
                    // we need the seg to END at `head`
                    chain.insert(
                        0,
                        if rev {
                            segs[i].reversed()
                        } else {
                            segs[i].clone()
                        },
                    );
                }
                None => break,
            }
        }

        if chain_closed(&chain, tol) {
            loops.push(chain);
        } else {
            open += 1;
            for i in taken {
                state[i] = 2;
            }
        }
    }

    (loops, open)
}

/// A chain is closed when its last endpoint meets its first start point and
/// it actually encloses something (guards against zero-length chains).
fn chain_closed(chain: &[Seg], tol: f64) -> bool {
    if chain.is_empty() {
        return false;
    }
    let total: f64 = chain.iter().map(|s| s.length()).sum();
    total > tol && chain.last().unwrap().end().dist(&chain[0].start()) <= tol
}

/// Closest free segment with an endpoint within `tol` of `at`.
///
/// `forward` = true looks for a segment that *starts* at `at` (reversing it
/// if only its end matches); `forward` = false looks for a segment that
/// *ends* at `at`. Returns (index, needs_reverse).
fn nearest_free(
    segs: &[Seg],
    state: &[u8],
    at: Pt,
    tol: f64,
    forward: bool,
) -> Option<(usize, bool)> {
    let mut best: Option<(usize, bool, f64)> = None;
    for (i, s) in segs.iter().enumerate() {
        if state[i] != 0 {
            continue;
        }
        let (d_direct, d_rev) = if forward {
            (at.dist(&s.start()), at.dist(&s.end()))
        } else {
            (at.dist(&s.end()), at.dist(&s.start()))
        };
        if d_direct <= tol && best.is_none_or(|(_, _, d)| d_direct < d) {
            best = Some((i, false, d_direct));
        }
        if d_rev <= tol && best.is_none_or(|(_, _, d)| d_rev < d) {
            best = Some((i, true, d_rev));
        }
    }
    best.map(|(i, rev, _)| (i, rev))
}

// ---------------------------------------------------------------------------
// entity conversion helpers
// ---------------------------------------------------------------------------

/// DXF ARC: always CCW from `start_angle` to `end_angle` (degrees, the end
/// may numerically wrap past 360). Equal angles = degenerate -> `None`.
fn arc_seg(a: &dxf::entities::Arc) -> Option<Seg> {
    if a.radius <= EPS || !a.start_angle.is_finite() || !a.end_angle.is_finite() {
        return None;
    }
    let sweep = (a.end_angle - a.start_angle).rem_euclid(360.0);
    if sweep < 1e-9 {
        return None;
    }
    Some(Seg::Arc {
        c: Pt::new(a.center.x, a.center.y),
        r: a.radius,
        start_deg: a.start_angle,
        sweep_deg: sweep,
    })
}

/// Turn `(x, y, bulge)` vertices into segments. The bulge stored on vertex
/// `i` describes the segment `i -> i+1`; when `closed`, the bulge on the
/// last vertex describes the closing segment back to vertex 0.
fn emit_polyline(pts: &[(f64, f64, f64)], closed: bool, out: &mut Vec<Seg>) {
    let n = pts.len();
    if n < 2 {
        return;
    }
    let count = if closed { n } else { n - 1 };
    for i in 0..count {
        let (x1, y1, bulge) = pts[i];
        let (x2, y2, _) = pts[(i + 1) % n];
        let a = Pt::new(x1, y1);
        let b = Pt::new(x2, y2);
        if a.dist(&b) <= EPS {
            continue;
        }
        match bulge_arc(a, b, bulge) {
            Some(s) => out.push(s),
            None => out.push(Seg::Line { a, b }),
        }
    }
}

/// Bulge-to-arc: `bulge = tan(sweep/4)`, positive = CCW, negative = CW.
/// Returns `None` when the bulge is effectively zero (i.e. a straight line).
fn bulge_arc(p1: Pt, p2: Pt, bulge: f64) -> Option<Seg> {
    if !bulge.is_finite() || bulge.abs() < 1e-12 {
        return None;
    }
    let d = p1.dist(&p2);
    if d <= EPS {
        return None;
    }
    let theta = 4.0 * bulge.atan(); // signed sweep, radians
    let half = theta / 2.0;
    let sin_half = half.sin();
    if sin_half.abs() < 1e-9 {
        return None;
    }
    let a = d / 2.0;
    let r = (a / sin_half).abs();
    // Distance from the chord midpoint to the centre, along the chord's
    // left normal: positive for |sweep| < 180 deg, negative beyond.
    let h = a / half.tan();
    if !h.is_finite() || !r.is_finite() {
        return None;
    }
    let ux = (p2.x - p1.x) / d;
    let uy = (p2.y - p1.y) / d;
    let cx = (p1.x + p2.x) / 2.0 - h * uy;
    let cy = (p1.y + p2.y) / 2.0 + h * ux;
    let c = Pt::new(cx, cy);
    let start_deg = (p1.y - cy).atan2(p1.x - cx).to_degrees();
    Some(Seg::Arc {
        c,
        r,
        start_deg,
        sweep_deg: theta.to_degrees(),
    })
}

/// Turn a linearized point chain into `Seg::Line`s (dropping duplicates).
fn emit_points(pts: &[Pt], out: &mut Vec<Seg>) {
    let mut prev: Option<Pt> = None;
    for p in pts {
        match prev {
            Some(a) if a.dist(p) <= EPS => {}
            Some(a) => {
                out.push(Seg::Line { a, b: *p });
                prev = Some(*p);
            }
            None => prev = Some(*p),
        }
    }
}

/// Upper bound on samples per spline, so a pathological knot vector can't
/// blow up parse time.
const MAX_SPLINE_SAMPLES: usize = 8192;

/// Linearize a SPLINE. Uses De Boor evaluation over the control points /
/// knot vector when they are consistent, otherwise falls back to the fit
/// points (or, last resort, the control polygon).
///
/// Sampling starts at `max(64, control_points * 8)` uniform steps and is
/// doubled until every chord's midpoint is within `tol` of the curve.
fn spline_points(sp: &dxf::entities::Spline, tol: f64) -> Vec<Pt> {
    let ctrl: Vec<Pt> = sp
        .control_points
        .iter()
        .map(|p| Pt::new(p.x, p.y))
        .collect();
    let n = ctrl.len();
    let knots = &sp.knot_values;
    let degree = if sp.degree_of_curve > 0 {
        sp.degree_of_curve as usize
    } else {
        3
    };

    if n >= 2 && degree < n && knots.len() == n + degree + 1 {
        let u0 = knots[degree];
        let u1 = knots[n];
        if u1 - u0 > 1e-12 {
            let weights: Vec<f64> = if sp.weight_values.len() == n {
                sp.weight_values.clone()
            } else {
                vec![1.0; n]
            };
            let at = |samples: usize| -> Vec<Pt> {
                (0..=samples)
                    .map(|i| {
                        let u = u0 + (u1 - u0) * (i as f64) / (samples as f64);
                        de_boor(degree, knots, &ctrl, &weights, u)
                    })
                    .collect()
            };

            let mut samples = (n * 8).max(64);
            let mut out = at(samples);
            while samples < MAX_SPLINE_SAMPLES {
                // worst deviation of a chord midpoint from the real curve
                let worst = (0..samples)
                    .map(|i| {
                        let u = u0 + (u1 - u0) * (i as f64 + 0.5) / (samples as f64);
                        let curve = de_boor(degree, knots, &ctrl, &weights, u);
                        let mid = Pt::new(
                            (out[i].x + out[i + 1].x) / 2.0,
                            (out[i].y + out[i + 1].y) / 2.0,
                        );
                        curve.dist(&mid)
                    })
                    .fold(0.0f64, f64::max);
                if worst <= tol {
                    break;
                }
                samples = (samples * 2).min(MAX_SPLINE_SAMPLES);
                out = at(samples);
            }
            return out;
        }
    }

    if sp.fit_points.len() >= 2 {
        return sp.fit_points.iter().map(|p| Pt::new(p.x, p.y)).collect();
    }
    ctrl
}

/// De Boor evaluation of a (possibly rational) B-spline at parameter `u`.
fn de_boor(degree: usize, knots: &[f64], ctrl: &[Pt], w: &[f64], u: f64) -> Pt {
    let n = ctrl.len();
    // knot span k with knots[k] <= u < knots[k+1], clamped to [degree, n-1]
    let k = if u >= knots[n] {
        n - 1
    } else {
        let mut i = degree;
        while i + 1 < n && !(knots[i] <= u && u < knots[i + 1]) {
            i += 1;
        }
        i.clamp(degree, n - 1)
    };

    let mut dx = vec![0.0f64; degree + 1];
    let mut dy = vec![0.0f64; degree + 1];
    let mut dw = vec![0.0f64; degree + 1];
    for j in 0..=degree {
        let idx = j + k - degree;
        let wi = if w[idx].is_finite() && w[idx] != 0.0 {
            w[idx]
        } else {
            1.0
        };
        dx[j] = ctrl[idx].x * wi;
        dy[j] = ctrl[idx].y * wi;
        dw[j] = wi;
    }

    for r in 1..=degree {
        for j in (r..=degree).rev() {
            let lo = knots[j + k - degree];
            let hi = knots[j + 1 + k - r];
            let denom = hi - lo;
            let alpha = if denom.abs() < 1e-12 {
                0.0
            } else {
                (u - lo) / denom
            };
            dx[j] = (1.0 - alpha) * dx[j - 1] + alpha * dx[j];
            dy[j] = (1.0 - alpha) * dy[j - 1] + alpha * dy[j];
            dw[j] = (1.0 - alpha) * dw[j - 1] + alpha * dw[j];
        }
    }

    let wf = if dw[degree].abs() < 1e-12 {
        1.0
    } else {
        dw[degree]
    };
    Pt::new(dx[degree] / wf, dy[degree] / wf)
}

/// Linearize an ELLIPSE honouring its start/end parameters. For a uniform
/// parameter sampling the worst-case chord error is `a * dt^2 / 8` with `a`
/// the semi-major length, which is exactly the circular estimate at r = a.
fn ellipse_points(el: &dxf::entities::Ellipse, tol: f64) -> Vec<Pt> {
    let cx = el.center.x;
    let cy = el.center.y;
    let mx = el.major_axis.x;
    let my = el.major_axis.y;
    let a = (mx * mx + my * my).sqrt();
    if a <= EPS || !a.is_finite() {
        return Vec::new();
    }
    let ratio = el.minor_axis_ratio.abs();
    // minor axis vector = left normal of the major axis, scaled by the ratio
    let nx = -my * ratio;
    let ny = mx * ratio;

    let t0 = el.start_parameter;
    let mut sweep = el.end_parameter - el.start_parameter;
    if !sweep.is_finite() || sweep.abs() <= 1e-12 {
        sweep = std::f64::consts::TAU;
    }
    if sweep < 0.0 {
        sweep += std::f64::consts::TAU;
    }
    if sweep > std::f64::consts::TAU {
        sweep = std::f64::consts::TAU;
    }

    let step = if a <= tol {
        std::f64::consts::FRAC_PI_2
    } else {
        2.0 * (1.0 - tol / a).acos()
    };
    let nseg = if step > 1e-12 {
        ((sweep / step).ceil() as usize).clamp(8, 8192)
    } else {
        8192
    };

    let mut out = Vec::with_capacity(nseg + 1);
    for i in 0..=nseg {
        let t = t0 + sweep * (i as f64) / (nseg as f64);
        let (s, c) = t.sin_cos();
        out.push(Pt::new(cx + mx * c + nx * s, cy + my * c + ny * s));
    }
    out
}

fn scaled_seg(s: &Seg, k: f64) -> Seg {
    match s {
        Seg::Line { a, b } => Seg::Line {
            a: Pt::new(a.x * k, a.y * k),
            b: Pt::new(b.x * k, b.y * k),
        },
        Seg::Arc {
            c,
            r,
            start_deg,
            sweep_deg,
        } => Seg::Arc {
            c: Pt::new(c.x * k, c.y * k),
            r: r * k,
            start_deg: *start_deg,
            sweep_deg: *sweep_deg,
        },
    }
}

/// $INSUNITS -> millimetres-per-drawing-unit. Anything that is not already
/// millimetres (or unitless) gets a warning.
fn unit_scale(units: Units, warnings: &mut Vec<String>) -> f64 {
    let (scale, label): (f64, &str) = match units {
        Units::Unitless | Units::Millimeters => (1.0, "mm"),
        Units::Inches => (25.4, "inches"),
        Units::Feet => (304.8, "feet"),
        Units::Miles => (1_609_344.0, "miles"),
        Units::Centimeters => (10.0, "centimeters"),
        Units::Meters => (1000.0, "meters"),
        Units::Kilometers => (1_000_000.0, "kilometers"),
        Units::Microinches => (2.54e-5, "microinches"),
        Units::Mils => (0.0254, "mils"),
        Units::Yards => (914.4, "yards"),
        Units::Angstroms => (1e-7, "angstroms"),
        Units::Nanometers => (1e-6, "nanometers"),
        Units::Microns => (1e-3, "microns"),
        Units::Decimeters => (100.0, "decimeters"),
        Units::Decameters => (10_000.0, "decameters"),
        Units::Hectometers => (100_000.0, "hectometers"),
        Units::USSurveyFeet => (304.800_609_601_219, "US survey feet"),
        Units::USSurveyInch => (25.400_050_800_101_6, "US survey inches"),
        Units::USSurveyYard => (914.401_828_803_658, "US survey yards"),
        Units::USSurveyMile => (1_609_347.218_694_44, "US survey miles"),
        _ => {
            warnings.push(
                "drawing uses an unsupported unit ($INSUNITS); treating coordinates as mm"
                    .to_string(),
            );
            return 1.0;
        }
    };
    if (scale - 1.0).abs() > 1e-12 {
        warnings.push(format!(
            "drawing units are {label}; scaled by {scale} to millimetres"
        ));
    }
    scale
}

fn entity_kind(e: &dxf::entities::EntityType) -> &'static str {
    use dxf::entities::EntityType as T;
    match e {
        T::Insert(_) => "INSERT",
        T::Text(_) => "TEXT",
        T::MText(_) => "MTEXT",
        T::Attribute(_) => "ATTRIB",
        T::AttributeDefinition(_) => "ATTDEF",
        T::ModelPoint(_) => "POINT",
        T::Solid(_) => "SOLID",
        T::Trace(_) => "TRACE",
        T::Face3D(_) => "3DFACE",
        T::Solid3D(_) => "3DSOLID",
        T::Region(_) => "REGION",
        T::Body(_) => "BODY",
        T::Leader(_) => "LEADER",
        T::MLine(_) => "MLINE",
        T::Ray(_) => "RAY",
        T::XLine(_) => "XLINE",
        T::Helix(_) => "HELIX",
        T::Image(_) => "IMAGE",
        T::Shape(_) => "SHAPE",
        T::Wipeout(_) => "WIPEOUT",
        T::Light(_) => "LIGHT",
        T::Section(_) => "SECTION",
        T::Tolerance(_) => "TOLERANCE",
        T::ArcAlignedText(_) => "ARCALIGNEDTEXT",
        T::RText(_) => "RTEXT",
        T::OleFrame(_) | T::Ole2Frame(_) => "OLEFRAME",
        T::ProxyEntity(_) => "ACAD_PROXY_ENTITY",
        T::DgnUnderlay(_) | T::DwfUnderlay(_) | T::PdfUnderlay(_) => "UNDERLAY",
        T::RotatedDimension(_)
        | T::RadialDimension(_)
        | T::DiameterDimension(_)
        | T::AngularThreePointDimension(_)
        | T::OrdinateDimension(_) => "DIMENSION",
        _ => "UNKNOWN",
    }
}

// ---------------------------------------------------------------------------
// local geometry helpers (deliberately NOT using geom.rs)
// ---------------------------------------------------------------------------

fn bbox_contains(outer: &(f64, f64, f64, f64), inner: &(f64, f64, f64, f64)) -> bool {
    // generous slack: the rings are linearizations, not exact curves
    let s = 1e-6;
    outer.0 - s <= inner.0
        && outer.1 - s <= inner.1
        && outer.2 + s >= inner.2
        && outer.3 + s >= inner.3
}

/// Even-odd ray cast (horizontal ray to +x).
fn point_in_ring(p: Pt, ring: &[Pt]) -> bool {
    let n = ring.len();
    if n < 3 {
        return false;
    }
    let mut inside = false;
    let mut j = n - 1;
    for i in 0..n {
        let a = ring[i];
        let b = ring[j];
        if (a.y > p.y) != (b.y > p.y) {
            let t = (p.y - a.y) / (b.y - a.y);
            let x = a.x + t * (b.x - a.x);
            if p.x < x {
                inside = !inside;
            }
        }
        j = i;
    }
    inside
}

#[cfg(test)]
mod tests {
    // Required tests (implementer: keep these, add more):
    // - roundtrip: build a Drawing with the `dxf` crate containing a
    //   10x10 LWPOLYLINE square + a CIRCLE hole r=2 inside, write to bytes,
    //   parse_dxf -> one Part with 1 hole, gross_area ~ 100,
    //   net_area ~ 100 - PI*4, normalized to bbox min (0,0).
    // - chain_segments closes a square given 4 unordered LINE segs, some
    //   reversed.
    // - two disjoint squares in one file -> two parts.
    // - a LWPOLYLINE with bulge=1 (semicircle) parses to an Arc seg.
    use super::*;
    use crate::model::ring_bbox;
    use dxf::entities::{Circle, Entity, EntityType, LwPolyline};
    use dxf::enums::AcadVersion;
    use dxf::{LwPolylineVertex, Point};

    const TOL: f64 = 0.05;

    /// `Contour::polyline` inscribes curves, so any area derived from a
    /// linearized ring lands slightly *under* the analytic value. Assert it
    /// never overshoots and stays within `rel` of the exact figure.
    fn assert_inscribed(actual: f64, exact: f64, rel: f64) {
        assert!(
            actual <= exact + 1e-9,
            "inscribed area {actual} overshot exact {exact}"
        );
        assert!(
            actual >= exact * (1.0 - rel),
            "inscribed area {actual} is more than {} under exact {exact}",
            rel * 100.0
        );
    }

    fn new_drawing() -> Drawing {
        let mut d = Drawing::new();
        // LWPOLYLINE needs R14+, $INSUNITS needs R2000+
        d.header.version = AcadVersion::R2010;
        d.header.default_drawing_units = Units::Millimeters;
        d
    }

    fn add_square(d: &mut Drawing, x: f64, y: f64, w: f64, h: f64) {
        let mut poly = LwPolyline::default();
        poly.set_is_closed(true);
        for (px, py) in [(x, y), (x + w, y), (x + w, y + h), (x, y + h)] {
            poly.vertices.push(LwPolylineVertex {
                x: px,
                y: py,
                ..Default::default()
            });
        }
        d.add_entity(Entity::new(EntityType::LwPolyline(poly)));
    }

    fn add_circle(d: &mut Drawing, x: f64, y: f64, r: f64) {
        let c = Circle {
            center: Point::new(x, y, 0.0),
            radius: r,
            ..Default::default()
        };
        d.add_entity(Entity::new(EntityType::Circle(c)));
    }

    fn to_bytes(d: &Drawing) -> Vec<u8> {
        let mut buf: Vec<u8> = Vec::new();
        d.save(&mut buf).expect("save drawing");
        buf
    }

    // --- required: roundtrip ------------------------------------------------
    #[test]
    fn roundtrip_square_with_circle_hole() {
        let mut d = new_drawing();
        add_square(&mut d, 5.0, 7.0, 10.0, 10.0);
        add_circle(&mut d, 10.0, 12.0, 2.0);
        let bytes = to_bytes(&d);

        let parsed = parse_dxf(&bytes, "plate.dxf", 3, 0.01).expect("parse");
        assert_eq!(parsed.parts.len(), 1, "warnings: {:?}", parsed.warnings);
        let p = &parsed.parts[0];
        assert_eq!(p.name, "plate.dxf");
        assert_eq!(p.quantity, 3);
        assert_eq!(p.holes.len(), 1);
        assert!(
            (p.gross_area - 100.0).abs() < 1e-9,
            "gross={}",
            p.gross_area
        );
        // hole is a linearized circle -> inscribed, hence slightly small
        let hole_area = p.gross_area - p.net_area;
        assert_inscribed(hole_area, std::f64::consts::PI * 4.0, 0.01);

        // normalized to bbox min (0,0)
        let (minx, miny, maxx, maxy) = ring_bbox(&p.outer_poly);
        assert!(
            minx.abs() < 1e-9 && miny.abs() < 1e-9,
            "min=({minx},{miny})"
        );
        assert!((maxx - 10.0).abs() < 1e-9 && (maxy - 10.0).abs() < 1e-9);
        // the hole moved with the outer: centre was (10,12), outer min (5,7)
        let (hx0, hy0, hx1, hy1) = ring_bbox(&p.holes[0].polyline(0.01));
        assert!(((hx0 + hx1) / 2.0 - 5.0).abs() < 1e-6);
        assert!(((hy0 + hy1) / 2.0 - 5.0).abs() < 1e-6);

        // outer CCW, hole CW
        assert!(ring_signed_area(&p.outer_poly) > 0.0);
        assert!(p.holes[0].signed_area(0.01) < 0.0);
    }

    // --- required: chaining -------------------------------------------------
    #[test]
    fn chain_segments_closes_unordered_square() {
        let a = Pt::new(0.0, 0.0);
        let b = Pt::new(10.0, 0.0);
        let c = Pt::new(10.0, 10.0);
        let e = Pt::new(0.0, 10.0);
        // shuffled, some reversed
        let segs = vec![
            Seg::Line { a: c, b },    // reversed b->c
            Seg::Line { a: e, b: c }, // reversed c->e
            Seg::Line { a, b },       // forward
            Seg::Line { a, b: e },    // reversed e->a
        ];
        let (loops, open) = chain_segments(segs);
        assert_eq!(open, 0);
        assert_eq!(loops.len(), 1);
        assert_eq!(loops[0].len(), 4);
        let ring = Contour {
            segs: loops[0].clone(),
        }
        .polyline(TOL);
        assert_eq!(ring.len(), 4);
        assert!((ring_signed_area(&ring).abs() - 100.0).abs() < 1e-9);
        // consecutive segments actually chain
        for w in loops[0].windows(2) {
            assert!(w[0].end().dist(&w[1].start()) < CHAIN_TOL);
        }
    }

    #[test]
    fn chain_segments_reports_open_chains() {
        let segs = vec![
            Seg::Line {
                a: Pt::new(0.0, 0.0),
                b: Pt::new(10.0, 0.0),
            },
            Seg::Line {
                a: Pt::new(10.0, 0.0),
                b: Pt::new(10.0, 10.0),
            },
        ];
        let (loops, open) = chain_segments(segs);
        assert!(loops.is_empty());
        assert_eq!(open, 1);
    }

    #[test]
    fn chain_segments_uses_loose_pass() {
        // gap of 0.2mm: too wide for CHAIN_TOL (0.05) but fine for
        // CHAIN_TOL_LOOSE (0.5)
        let segs = vec![
            Seg::Line {
                a: Pt::new(0.0, 0.0),
                b: Pt::new(10.0, 0.0),
            },
            Seg::Line {
                a: Pt::new(10.0, 0.2),
                b: Pt::new(10.0, 10.0),
            },
            Seg::Line {
                a: Pt::new(10.0, 10.0),
                b: Pt::new(0.0, 10.0),
            },
            Seg::Line {
                a: Pt::new(0.0, 10.0),
                b: Pt::new(0.0, 0.0),
            },
        ];
        let (loops, open) = chain_segments(segs);
        assert_eq!(open, 0);
        assert_eq!(loops.len(), 1);
        assert_eq!(loops[0].len(), 4);
    }

    #[test]
    fn chain_segments_single_full_circle_is_a_loop() {
        let segs = vec![Seg::Arc {
            c: Pt::new(0.0, 0.0),
            r: 5.0,
            start_deg: 0.0,
            sweep_deg: 360.0,
        }];
        let (loops, open) = chain_segments(segs);
        assert_eq!(open, 0);
        assert_eq!(loops.len(), 1);
        assert_eq!(loops[0].len(), 1);
    }

    // --- required: two disjoint squares -------------------------------------
    #[test]
    fn two_disjoint_squares_make_two_parts() {
        let mut d = new_drawing();
        add_square(&mut d, 0.0, 0.0, 10.0, 10.0);
        add_square(&mut d, 50.0, 50.0, 20.0, 20.0);
        let bytes = to_bytes(&d);

        let parsed = parse_dxf(&bytes, "two.dxf", 1, 0.1).expect("parse");
        assert_eq!(parsed.parts.len(), 2, "warnings: {:?}", parsed.warnings);
        assert_eq!(parsed.parts[0].name, "two.dxf");
        assert_eq!(parsed.parts[1].name, "two.dxf#2");
        let mut areas: Vec<f64> = parsed.parts.iter().map(|p| p.gross_area).collect();
        areas.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert!((areas[0] - 100.0).abs() < 1e-6);
        assert!((areas[1] - 400.0).abs() < 1e-6);
        // both normalized to (0,0)
        for p in &parsed.parts {
            let (minx, miny, _, _) = ring_bbox(&p.outer_poly);
            assert!(minx.abs() < 1e-9 && miny.abs() < 1e-9);
        }
    }

    // --- required: bulge ----------------------------------------------------
    #[test]
    fn lwpolyline_bulge_one_makes_semicircle_arc() {
        // (0,0) --bulge 1--> (2,0) --line--> back
        let mut d = new_drawing();
        let mut poly = LwPolyline::default();
        poly.set_is_closed(true);
        poly.vertices.push(LwPolylineVertex {
            x: 0.0,
            y: 0.0,
            bulge: 1.0,
            ..Default::default()
        });
        poly.vertices.push(LwPolylineVertex {
            x: 2.0,
            y: 0.0,
            ..Default::default()
        });
        d.add_entity(Entity::new(EntityType::LwPolyline(poly)));
        let bytes = to_bytes(&d);

        let parsed = parse_dxf(&bytes, "half.dxf", 1, 0.002).expect("parse");
        assert_eq!(parsed.parts.len(), 1, "warnings: {:?}", parsed.warnings);
        let p = &parsed.parts[0];
        let arcs: Vec<&Seg> = p
            .outer
            .segs
            .iter()
            .filter(|s| matches!(s, Seg::Arc { .. }))
            .collect();
        assert_eq!(arcs.len(), 1, "segs = {:?}", p.outer.segs);
        match arcs[0] {
            Seg::Arc { r, sweep_deg, .. } => {
                assert!((r - 1.0).abs() < 1e-9, "r={r}");
                assert!((sweep_deg.abs() - 180.0).abs() < 1e-9, "sweep={sweep_deg}");
            }
            _ => unreachable!(),
        }
        // half disc of r=1
        assert_inscribed(p.gross_area, std::f64::consts::PI / 2.0, 0.005);
    }

    #[test]
    fn bulge_arc_geometry_matches_expectation() {
        // CCW semicircle from (0,0) to (2,0): centre (1,0), r=1,
        // starting at 180 deg sweeping +180 deg (dipping to y<0).
        let s = bulge_arc(Pt::new(0.0, 0.0), Pt::new(2.0, 0.0), 1.0).unwrap();
        match s {
            Seg::Arc {
                c, r, sweep_deg, ..
            } => {
                assert!(c.dist(&Pt::new(1.0, 0.0)) < 1e-9);
                assert!((r - 1.0).abs() < 1e-9);
                assert!((sweep_deg - 180.0).abs() < 1e-9);
            }
            _ => panic!("expected arc"),
        }
        assert!(s.start().dist(&Pt::new(0.0, 0.0)) < 1e-9);
        assert!(s.point_at(s.length() / 2.0).dist(&Pt::new(1.0, -1.0)) < 1e-9);
        assert!(s.end().dist(&Pt::new(2.0, 0.0)) < 1e-9);

        // 90 deg CCW quarter: centre must sit left of the chord.
        let b = (std::f64::consts::FRAC_PI_8).tan();
        let q = bulge_arc(Pt::new(0.0, 0.0), Pt::new(2.0, 0.0), b).unwrap();
        match q {
            Seg::Arc { c, sweep_deg, .. } => {
                assert!(c.dist(&Pt::new(1.0, 1.0)) < 1e-9, "c={c:?}");
                assert!((sweep_deg - 90.0).abs() < 1e-9);
            }
            _ => panic!("expected arc"),
        }

        // negative bulge = CW
        let cw = bulge_arc(Pt::new(0.0, 0.0), Pt::new(2.0, 0.0), -1.0).unwrap();
        match cw {
            Seg::Arc { sweep_deg, c, .. } => {
                assert!((sweep_deg + 180.0).abs() < 1e-9);
                assert!(c.dist(&Pt::new(1.0, 0.0)) < 1e-9);
            }
            _ => panic!("expected arc"),
        }
        assert!(cw.point_at(cw.length() / 2.0).dist(&Pt::new(1.0, 1.0)) < 1e-9);

        assert!(bulge_arc(Pt::new(0.0, 0.0), Pt::new(2.0, 0.0), 0.0).is_none());
    }

    // --- units --------------------------------------------------------------
    #[test]
    fn inch_drawings_are_scaled() {
        let mut d = new_drawing();
        d.header.default_drawing_units = Units::Inches;
        add_square(&mut d, 0.0, 0.0, 1.0, 1.0);
        let bytes = to_bytes(&d);

        let parsed = parse_dxf(&bytes, "inch.dxf", 1, 0.1).expect("parse");
        assert_eq!(parsed.parts.len(), 1);
        assert!(
            (parsed.parts[0].gross_area - 25.4 * 25.4).abs() < 1e-6,
            "area={}",
            parsed.parts[0].gross_area
        );
        assert!(
            parsed.warnings.iter().any(|w| w.contains("inches")),
            "warnings={:?}",
            parsed.warnings
        );
    }

    // --- arcs ---------------------------------------------------------------
    #[test]
    fn arc_sweep_wraps_past_360() {
        let mut a = dxf::entities::Arc {
            center: Point::new(0.0, 0.0, 0.0),
            radius: 5.0,
            start_angle: 350.0,
            end_angle: 370.0, // wraps
            ..Default::default()
        };
        let s = arc_seg(&a).unwrap();
        match s {
            Seg::Arc {
                start_deg,
                sweep_deg,
                ..
            } => {
                assert!((start_deg - 350.0).abs() < 1e-9);
                assert!((sweep_deg - 20.0).abs() < 1e-9);
            }
            _ => panic!("expected arc"),
        }

        // negative wrap
        a.start_angle = 350.0;
        a.end_angle = 10.0;
        match arc_seg(&a).unwrap() {
            Seg::Arc { sweep_deg, .. } => assert!((sweep_deg - 20.0).abs() < 1e-9),
            _ => panic!("expected arc"),
        }
    }

    #[test]
    fn degenerate_arc_is_skipped() {
        let mut a = dxf::entities::Arc {
            center: Point::new(0.0, 0.0, 0.0),
            radius: 5.0,
            start_angle: 30.0,
            end_angle: 30.0,
            ..Default::default()
        };
        assert!(arc_seg(&a).is_none());

        // zero radius
        a.radius = 0.0;
        a.end_angle = 90.0;
        assert!(arc_seg(&a).is_none());
    }

    #[test]
    fn circle_becomes_one_full_arc_seg() {
        let mut d = new_drawing();
        add_circle(&mut d, 3.0, 4.0, 5.0);
        let bytes = to_bytes(&d);
        let parsed = parse_dxf(&bytes, "disc.dxf", 1, 0.01).expect("parse");
        assert_eq!(parsed.parts.len(), 1);
        let p = &parsed.parts[0];
        assert_eq!(p.outer.segs.len(), 1);
        match p.outer.segs[0] {
            Seg::Arc { r, sweep_deg, .. } => {
                assert!((r - 5.0).abs() < 1e-9);
                assert!((sweep_deg.abs() - 360.0).abs() < 1e-9);
            }
            _ => panic!("expected arc"),
        }
        assert_inscribed(p.gross_area, std::f64::consts::PI * 25.0, 0.005);
    }

    // --- splines ------------------------------------------------------------
    /// Cubic Bezier (0,0) -> (0,10) -> (10,10) -> (10,0) as a clamped spline.
    fn bezier_spline() -> dxf::entities::Spline {
        dxf::entities::Spline {
            degree_of_curve: 3,
            control_points: vec![
                Point::new(0.0, 0.0, 0.0),
                Point::new(0.0, 10.0, 0.0),
                Point::new(10.0, 10.0, 0.0),
                Point::new(10.0, 0.0, 0.0),
            ],
            knot_values: vec![0.0, 0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 1.0],
            ..Default::default()
        }
    }

    fn ellipse(semi_major: f64, ratio: f64) -> dxf::entities::Ellipse {
        dxf::entities::Ellipse {
            center: Point::new(0.0, 0.0, 0.0),
            major_axis: dxf::Vector::new(semi_major, 0.0, 0.0),
            minor_axis_ratio: ratio,
            start_parameter: 0.0,
            end_parameter: std::f64::consts::TAU,
            ..Default::default()
        }
    }

    #[test]
    fn de_boor_evaluates_cubic_bezier() {
        let sp = bezier_spline();
        let pts = spline_points(&sp, 0.05);
        assert!(pts.len() >= 65);
        assert!(pts[0].dist(&Pt::new(0.0, 0.0)) < 1e-9);
        assert!(pts.last().unwrap().dist(&Pt::new(10.0, 0.0)) < 1e-9);
        // Bezier at t = 0.5 -> (5.0, 7.5)
        let mid = pts[pts.len() / 2];
        assert!(mid.dist(&Pt::new(5.0, 7.5)) < 1e-9, "mid={mid:?}");
    }

    #[test]
    fn spline_sampling_refines_until_tolerance_is_met() {
        // A metre-scale Bezier: the base 64 samples are not enough for a
        // 0.001 mm chord budget, so the refinement loop has to kick in.
        let sp = dxf::entities::Spline {
            degree_of_curve: 3,
            control_points: vec![
                Point::new(0.0, 0.0, 0.0),
                Point::new(0.0, 1000.0, 0.0),
                Point::new(1000.0, 1000.0, 0.0),
                Point::new(1000.0, 0.0, 0.0),
            ],
            knot_values: vec![0.0, 0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 1.0],
            ..Default::default()
        };
        let tol = 0.001;
        let pts = spline_points(&sp, tol);
        assert!(pts.len() > 65, "expected refinement, got {} pts", pts.len());

        // exact Bezier evaluation, for an independent error check
        let bez = |t: f64| {
            let mt = 1.0 - t;
            let (b0, b1, b2, b3) = (mt * mt * mt, 3.0 * mt * mt * t, 3.0 * mt * t * t, t * t * t);
            Pt::new(
                b2 * 1000.0 + b3 * 1000.0,
                (b1 + b2) * 1000.0 + (b0 + b3) * 0.0,
            )
        };
        let n = pts.len() - 1;
        for i in 0..n {
            let mid = Pt::new(
                (pts[i].x + pts[i + 1].x) / 2.0,
                (pts[i].y + pts[i + 1].y) / 2.0,
            );
            let on_curve = bez((i as f64 + 0.5) / n as f64);
            assert!(
                on_curve.dist(&mid) <= tol + 1e-9,
                "chord error {} > {tol}",
                on_curve.dist(&mid)
            );
        }
    }

    #[test]
    fn spline_falls_back_to_fit_points() {
        let sp = dxf::entities::Spline {
            degree_of_curve: 3,
            fit_points: vec![
                Point::new(0.0, 0.0, 0.0),
                Point::new(5.0, 5.0, 0.0),
                Point::new(10.0, 0.0, 0.0),
            ],
            ..Default::default()
        };
        let pts = spline_points(&sp, 0.05);
        assert_eq!(pts.len(), 3);
        assert!(pts[1].dist(&Pt::new(5.0, 5.0)) < 1e-9);
    }

    #[test]
    fn spline_closes_a_part_together_with_lines() {
        // Bezier bulge from (0,0) to (10,0), then a straight line back.
        let mut d = new_drawing();
        d.add_entity(Entity::new(EntityType::Spline(bezier_spline())));
        let line = dxf::entities::Line {
            p1: Point::new(10.0, 0.0, 0.0),
            p2: Point::new(0.0, 0.0, 0.0),
            ..Default::default()
        };
        d.add_entity(Entity::new(EntityType::Line(line)));
        let bytes = to_bytes(&d);

        let parsed = parse_dxf(&bytes, "spline.dxf", 1, 0.05).expect("parse");
        assert_eq!(parsed.parts.len(), 1, "warnings: {:?}", parsed.warnings);
        // exact area enclosed by this cubic Bezier + its chord is 60
        // (1/2 * integral of x*y' - y*x' over t in [0,1]).
        assert_inscribed(parsed.parts[0].gross_area, 60.0, 0.005);
    }

    // --- ellipses -----------------------------------------------------------
    #[test]
    fn full_ellipse_area_is_correct() {
        let mut d = new_drawing();
        d.add_entity(Entity::new(EntityType::Ellipse(ellipse(10.0, 0.5))));
        let bytes = to_bytes(&d);

        let parsed = parse_dxf(&bytes, "ell.dxf", 1, 0.01).expect("parse");
        assert_eq!(parsed.parts.len(), 1, "warnings: {:?}", parsed.warnings);
        assert_inscribed(
            parsed.parts[0].gross_area,
            std::f64::consts::PI * 10.0 * 5.0,
            0.005,
        );
    }

    #[test]
    fn ellipse_chord_error_respects_tolerance() {
        let el = ellipse(50.0, 1.0); // degenerates to a circle of r=50
        let tol = 0.05;
        let pts = ellipse_points(&el, tol);
        // every chord midpoint must be within tol of the true circle
        for w in pts.windows(2) {
            let mx = (w[0].x + w[1].x) / 2.0;
            let my = (w[0].y + w[1].y) / 2.0;
            let err = 50.0 - (mx * mx + my * my).sqrt();
            assert!(err <= tol + 1e-9, "chord error {err} > {tol}");
        }
    }

    // --- containment --------------------------------------------------------
    #[test]
    fn depth_two_ring_starts_a_new_part() {
        let mut d = new_drawing();
        add_square(&mut d, 0.0, 0.0, 100.0, 100.0); // depth 0
        add_square(&mut d, 10.0, 10.0, 80.0, 80.0); // depth 1 -> hole
        add_square(&mut d, 20.0, 20.0, 60.0, 60.0); // depth 2 -> new part
        add_square(&mut d, 30.0, 30.0, 40.0, 40.0); // depth 3 -> its hole
        let bytes = to_bytes(&d);

        let parsed = parse_dxf(&bytes, "nested.dxf", 1, 0.1).expect("parse");
        assert_eq!(parsed.parts.len(), 2, "warnings: {:?}", parsed.warnings);
        let a = &parsed.parts[0];
        let b = &parsed.parts[1];
        assert!((a.gross_area - 10_000.0).abs() < 1e-6);
        assert_eq!(a.holes.len(), 1);
        assert!((a.net_area - (10_000.0 - 6_400.0)).abs() < 1e-6);
        assert!((b.gross_area - 3_600.0).abs() < 1e-6);
        assert_eq!(b.holes.len(), 1);
        assert!((b.net_area - (3_600.0 - 1_600.0)).abs() < 1e-6);
    }

    #[test]
    fn point_in_ring_ray_cast() {
        let ring = vec![
            Pt::new(0.0, 0.0),
            Pt::new(10.0, 0.0),
            Pt::new(10.0, 10.0),
            Pt::new(0.0, 10.0),
        ];
        assert!(point_in_ring(Pt::new(5.0, 5.0), &ring));
        assert!(!point_in_ring(Pt::new(15.0, 5.0), &ring));
        assert!(!point_in_ring(Pt::new(-1.0, 5.0), &ring));
        assert!(!point_in_ring(Pt::new(5.0, 20.0), &ring));
    }

    // --- unsupported entities ----------------------------------------------
    #[test]
    fn unsupported_entities_warn_and_are_skipped() {
        let mut d = new_drawing();
        add_square(&mut d, 0.0, 0.0, 10.0, 10.0);
        let ins = dxf::entities::Insert {
            name: "BLK".to_string(),
            location: Point::new(0.0, 0.0, 0.0),
            ..Default::default()
        };
        d.add_entity(Entity::new(EntityType::Insert(ins)));
        let txt = dxf::entities::Text {
            value: "hello".to_string(),
            ..Default::default()
        };
        d.add_entity(Entity::new(EntityType::Text(txt)));
        let bytes = to_bytes(&d);

        let parsed = parse_dxf(&bytes, "mixed.dxf", 1, 0.1).expect("parse");
        assert_eq!(parsed.parts.len(), 1);
        assert!(
            parsed.warnings.iter().any(|w| w.contains("INSERT")),
            "warnings={:?}",
            parsed.warnings
        );
        assert!(
            parsed.warnings.iter().any(|w| w.contains("TEXT")),
            "warnings={:?}",
            parsed.warnings
        );
    }

    #[test]
    fn open_contours_produce_a_single_warning_and_an_error_when_nothing_closes() {
        let mut d = new_drawing();
        for (p1, p2) in [
            ((0.0, 0.0), (10.0, 0.0)),
            ((10.0, 0.0), (10.0, 10.0)),
            ((10.0, 10.0), (0.0, 10.0)),
            // missing the closing edge
        ] {
            let line = dxf::entities::Line {
                p1: Point::new(p1.0, p1.1, 0.0),
                p2: Point::new(p2.0, p2.1, 0.0),
                ..Default::default()
            };
            d.add_entity(Entity::new(EntityType::Line(line)));
        }
        let bytes = to_bytes(&d);
        match parse_dxf(&bytes, "open.dxf", 1, 0.1) {
            Ok(p) => panic!("expected an error, got {} parts", p.parts.len()),
            Err(e) => assert!(e.to_string().contains("no closed contours"), "{e}"),
        }
    }

    #[test]
    fn garbage_bytes_error_out() {
        match parse_dxf(b"this is not a dxf file at all", "junk.dxf", 1, 0.1) {
            Ok(p) => panic!("expected an error, got {} parts", p.parts.len()),
            Err(e) => assert!(!e.to_string().is_empty()),
        }
    }

    #[test]
    fn polyline_heavy_variant_is_supported() {
        let mut d = new_drawing();
        let mut poly = dxf::entities::Polyline::default();
        poly.set_is_closed(true);
        let verts = [(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)];
        for (x, y) in verts {
            let v = dxf::entities::Vertex::new(Point::new(x, y, 0.0));
            poly.add_vertex(&mut d, v);
        }
        d.add_entity(Entity::new(EntityType::Polyline(poly)));
        let bytes = to_bytes(&d);

        let parsed = parse_dxf(&bytes, "heavy.dxf", 1, 0.1).expect("parse");
        assert_eq!(parsed.parts.len(), 1, "warnings: {:?}", parsed.warnings);
        assert!((parsed.parts[0].gross_area - 100.0).abs() < 1e-6);
    }
}
