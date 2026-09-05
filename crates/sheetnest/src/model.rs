//! Core data model shared by all modules.
//!
//! Conventions (do not change):
//! - All lengths are in millimeters, all angles in degrees.
//! - Rotation is counter-clockwise about the origin (0,0):
//!   x' = x*cos - y*sin, y' = x*sin + y*cos.
//! - A part placement means: rotate the part's local geometry by
//!   `rotation_deg` about the origin, then translate by `(dx, dy)`.
//! - Closed rings are `Vec<Pt>` with NO repeated last point.
//! - Outer contours are stored CCW, holes CW (normalized at parse time).

use serde::{Deserialize, Serialize};

pub const CHAIN_TOL: f64 = 0.05; // endpoint chaining tolerance, mm
pub const CHAIN_TOL_LOOSE: f64 = 0.5; // fallback chaining tolerance, mm

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Pt {
    pub x: f64,
    pub y: f64,
}

impl Pt {
    pub fn new(x: f64, y: f64) -> Self {
        Pt { x, y }
    }
    pub fn dist(&self, o: &Pt) -> f64 {
        ((self.x - o.x).powi(2) + (self.y - o.y).powi(2)).sqrt()
    }
    pub fn rotated(&self, deg: f64) -> Pt {
        let r = deg.to_radians();
        let (s, c) = r.sin_cos();
        Pt::new(self.x * c - self.y * s, self.x * s + self.y * c)
    }
    pub fn translated(&self, dx: f64, dy: f64) -> Pt {
        Pt::new(self.x + dx, self.y + dy)
    }
}

/// Exact geometry segment. Arcs keep their analytic form so the output DXF
/// contains true arcs (unlike SVGnest which emits dense polylines).
///
/// `sweep_deg > 0` means CCW, `< 0` means CW. `|sweep_deg| <= 360`.
#[derive(Debug, Clone, PartialEq)]
pub enum Seg {
    Line {
        a: Pt,
        b: Pt,
    },
    Arc {
        c: Pt,
        r: f64,
        start_deg: f64,
        sweep_deg: f64,
    },
}

impl Seg {
    pub fn start(&self) -> Pt {
        match self {
            Seg::Line { a, .. } => *a,
            Seg::Arc {
                c, r, start_deg, ..
            } => {
                let rad = start_deg.to_radians();
                Pt::new(c.x + r * rad.cos(), c.y + r * rad.sin())
            }
        }
    }

    pub fn end(&self) -> Pt {
        match self {
            Seg::Line { b, .. } => *b,
            Seg::Arc {
                c,
                r,
                start_deg,
                sweep_deg,
            } => {
                let rad = (start_deg + sweep_deg).to_radians();
                Pt::new(c.x + r * rad.cos(), c.y + r * rad.sin())
            }
        }
    }

    pub fn length(&self) -> f64 {
        match self {
            Seg::Line { a, b } => a.dist(b),
            Seg::Arc { r, sweep_deg, .. } => r * sweep_deg.to_radians().abs(),
        }
    }

    /// Point at arc-length distance `d` from the start (0 <= d <= length).
    pub fn point_at(&self, d: f64) -> Pt {
        match self {
            Seg::Line { a, b } => {
                let len = a.dist(b);
                if len < 1e-12 {
                    return *a;
                }
                let t = (d / len).clamp(0.0, 1.0);
                Pt::new(a.x + (b.x - a.x) * t, a.y + (b.y - a.y) * t)
            }
            Seg::Arc {
                c,
                r,
                start_deg,
                sweep_deg,
            } => {
                let len = self.length();
                let t = if len < 1e-12 {
                    0.0
                } else {
                    (d / len).clamp(0.0, 1.0)
                };
                let ang = (start_deg + sweep_deg * t).to_radians();
                Pt::new(c.x + r * ang.cos(), c.y + r * ang.sin())
            }
        }
    }

    /// Split at arc-length distance `d` from the start. Both halves keep the
    /// original direction. Caller guarantees 0 < d < length.
    pub fn split_at(&self, d: f64) -> (Seg, Seg) {
        match self {
            Seg::Line { a, b } => {
                let m = self.point_at(d);
                (Seg::Line { a: *a, b: m }, Seg::Line { a: m, b: *b })
            }
            Seg::Arc {
                c,
                r,
                start_deg,
                sweep_deg,
            } => {
                let len = self.length();
                let t = (d / len).clamp(0.0, 1.0);
                let mid = start_deg + sweep_deg * t;
                (
                    Seg::Arc {
                        c: *c,
                        r: *r,
                        start_deg: *start_deg,
                        sweep_deg: sweep_deg * t,
                    },
                    Seg::Arc {
                        c: *c,
                        r: *r,
                        start_deg: mid,
                        sweep_deg: sweep_deg * (1.0 - t),
                    },
                )
            }
        }
    }

    pub fn reversed(&self) -> Seg {
        match self {
            Seg::Line { a, b } => Seg::Line { a: *b, b: *a },
            Seg::Arc {
                c,
                r,
                start_deg,
                sweep_deg,
            } => Seg::Arc {
                c: *c,
                r: *r,
                start_deg: start_deg + sweep_deg,
                sweep_deg: -sweep_deg,
            },
        }
    }

    /// Rotate about origin by `deg`, then translate by (dx, dy).
    pub fn transformed(&self, deg: f64, dx: f64, dy: f64) -> Seg {
        match self {
            Seg::Line { a, b } => Seg::Line {
                a: a.rotated(deg).translated(dx, dy),
                b: b.rotated(deg).translated(dx, dy),
            },
            Seg::Arc {
                c,
                r,
                start_deg,
                sweep_deg,
            } => Seg::Arc {
                c: c.rotated(deg).translated(dx, dy),
                r: *r,
                start_deg: start_deg + deg,
                sweep_deg: *sweep_deg,
            },
        }
    }

    /// Append the linearized form of this segment to `out`, EXCLUDING the
    /// start point (so consecutive segments chain without duplicates).
    /// `tol` is the max chord error in mm.
    pub fn linearize_into(&self, tol: f64, out: &mut Vec<Pt>) {
        match self {
            Seg::Line { b, .. } => out.push(*b),
            Seg::Arc {
                c,
                r,
                start_deg,
                sweep_deg,
            } => {
                let sweep_rad = sweep_deg.to_radians();
                // max angular step for chord error <= tol
                let step = if *r <= tol {
                    std::f64::consts::FRAC_PI_2
                } else {
                    2.0 * (1.0 - tol / r).acos()
                };
                let n = ((sweep_rad.abs() / step).ceil() as usize).max(1);
                for i in 1..=n {
                    let ang = (start_deg + sweep_deg * (i as f64) / (n as f64)).to_radians();
                    out.push(Pt::new(c.x + r * ang.cos(), c.y + r * ang.sin()));
                }
            }
        }
    }
}

/// A closed loop of segments. Invariant: `seg[i].end() ~= seg[i+1].start()`
/// (within CHAIN_TOL) and last.end() ~= first.start().
#[derive(Debug, Clone, PartialEq)]
pub struct Contour {
    pub segs: Vec<Seg>,
}

impl Contour {
    pub fn perimeter(&self) -> f64 {
        self.segs.iter().map(|s| s.length()).sum()
    }

    /// Linearize into a closed ring (no repeated last point).
    pub fn polyline(&self, tol: f64) -> Vec<Pt> {
        let mut out = Vec::new();
        if self.segs.is_empty() {
            return out;
        }
        out.push(self.segs[0].start());
        for s in &self.segs {
            s.linearize_into(tol, &mut out);
        }
        // drop closing duplicate if present
        if out.len() >= 2 && out[0].dist(out.last().unwrap()) < 1e-6 {
            out.pop();
        }
        out
    }

    /// Shoelace signed area of the linearized ring (CCW positive).
    pub fn signed_area(&self, tol: f64) -> f64 {
        ring_signed_area(&self.polyline(tol))
    }

    /// Reverse the loop direction in place.
    pub fn reverse(&mut self) {
        self.segs.reverse();
        for s in &mut self.segs {
            *s = s.reversed();
        }
    }

    pub fn transformed(&self, deg: f64, dx: f64, dy: f64) -> Contour {
        Contour {
            segs: self
                .segs
                .iter()
                .map(|s| s.transformed(deg, dx, dy))
                .collect(),
        }
    }
}

pub fn ring_signed_area(ring: &[Pt]) -> f64 {
    let n = ring.len();
    if n < 3 {
        return 0.0;
    }
    let mut a = 0.0;
    for i in 0..n {
        let p = &ring[i];
        let q = &ring[(i + 1) % n];
        a += p.x * q.y - q.x * p.y;
    }
    a / 2.0
}

pub fn ring_bbox(ring: &[Pt]) -> (f64, f64, f64, f64) {
    let mut minx = f64::INFINITY;
    let mut miny = f64::INFINITY;
    let mut maxx = f64::NEG_INFINITY;
    let mut maxy = f64::NEG_INFINITY;
    for p in ring {
        minx = minx.min(p.x);
        miny = miny.min(p.y);
        maxx = maxx.max(p.x);
        maxy = maxy.max(p.y);
    }
    (minx, miny, maxx, maxy)
}

/// Loops with |area| below this are noise, not geometry.
pub const MIN_LOOP_AREA: f64 = 1e-6;

/// A part to nest: one outer contour plus zero or more holes, in local
/// coordinates (normalized so the outer bbox min corner is at (0,0)).
///
/// Parts have no id of their own: a part is identified by its index in the
/// slice handed to [`crate::nest`], and [`Placement::part_id`] is that index.
///
/// Build one with [`Part::from_polygon`] (plain vertex rings) or
/// [`Part::from_contours`] (exact lines and arcs); with the `dxf` feature,
/// [`crate::dxf::parse_dxf`] does it from a drawing.
#[derive(Debug, Clone)]
pub struct Part {
    pub name: String,
    pub quantity: u32,
    pub outer: Contour,
    pub holes: Vec<Contour>,
    /// Outer ring linearized at the tolerance the part was built with.
    pub outer_poly: Vec<Pt>,
    /// abs area of the outer ring.
    pub gross_area: f64,
    /// outer area minus hole areas (for utilization stats).
    pub net_area: f64,
}

impl Part {
    /// Build a part from exact contours and normalize it: the outer loop is
    /// made CCW and holes CW, everything is translated so the outer bbox min
    /// corner sits at (0,0), and the cached polyline/areas are computed at
    /// `curve_tol` (max chord error in mm; anything non-positive means 0.25).
    ///
    /// Holes are taken as given; nothing checks that they lie inside the
    /// outer contour. Errors when the outer contour is degenerate.
    pub fn from_contours(
        name: impl Into<String>,
        quantity: u32,
        outer: Contour,
        holes: Vec<Contour>,
        curve_tol: f64,
    ) -> anyhow::Result<Part> {
        let curve_tol = if curve_tol.is_finite() && curve_tol > 1e-6 {
            curve_tol
        } else {
            0.25
        };
        let name = name.into();
        let mut outer = outer;
        let ring = outer.polyline(curve_tol);
        if ring.len() < 3 || ring_signed_area(&ring).abs() < MIN_LOOP_AREA {
            anyhow::bail!("part {name:?}: outer contour is degenerate");
        }
        if ring_signed_area(&ring) < 0.0 {
            outer.reverse();
        }
        let mut holes = holes;
        for h in &mut holes {
            if h.signed_area(curve_tol) > 0.0 {
                h.reverse();
            }
        }

        // translate so the outer bbox min corner sits at (0,0)
        let (minx, miny, _, _) = ring_bbox(&ring);
        let outer = outer.transformed(0.0, -minx, -miny);
        let holes: Vec<Contour> = holes
            .into_iter()
            .map(|h| h.transformed(0.0, -minx, -miny))
            .collect();

        let outer_poly = outer.polyline(curve_tol);
        let gross_area = ring_signed_area(&outer_poly).abs();
        let hole_area: f64 = holes
            .iter()
            .map(|h| ring_signed_area(&h.polyline(curve_tol)).abs())
            .sum();

        Ok(Part {
            name,
            quantity,
            outer,
            holes,
            outer_poly,
            gross_area,
            net_area: (gross_area - hole_area).max(0.0),
        })
    }

    /// Build a part from plain vertex rings (straight edges only). Rings
    /// may be given in either winding order, with or without a repeated
    /// closing vertex.
    pub fn from_polygon(
        name: impl Into<String>,
        quantity: u32,
        outer: &[Pt],
        holes: &[Vec<Pt>],
    ) -> anyhow::Result<Part> {
        let name = name.into();
        let outer_c = ring_to_contour(outer)
            .ok_or_else(|| anyhow::anyhow!("part {name:?}: outer ring needs 3+ points"))?;
        let mut hole_cs = Vec::with_capacity(holes.len());
        for (i, h) in holes.iter().enumerate() {
            hole_cs.push(
                ring_to_contour(h)
                    .ok_or_else(|| anyhow::anyhow!("part {name:?}: hole {i} needs 3+ points"))?,
            );
        }
        Part::from_contours(name, quantity, outer_c, hole_cs, 0.25)
    }
}

/// Vertex ring -> contour of line segments. Drops a repeated closing point
/// and zero-length edges. `None` when fewer than 3 distinct points remain.
fn ring_to_contour(ring: &[Pt]) -> Option<Contour> {
    let mut pts: Vec<Pt> = Vec::with_capacity(ring.len());
    for p in ring {
        if !p.x.is_finite() || !p.y.is_finite() {
            return None;
        }
        if pts.last().is_none_or(|q: &Pt| q.dist(p) > 1e-9) {
            pts.push(*p);
        }
    }
    while pts.len() >= 2 && pts[0].dist(pts.last().unwrap()) <= 1e-9 {
        pts.pop();
    }
    if pts.len() < 3 {
        return None;
    }
    let n = pts.len();
    Some(Contour {
        segs: (0..n)
            .map(|i| Seg::Line {
                a: pts[i],
                b: pts[(i + 1) % n],
            })
            .collect(),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RotationMode {
    /// 0 / 90 / 180 / 270 only.
    Orthogonal,
    /// Multiples of `rotation_step_deg`.
    Free,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TabConfig {
    pub enabled: bool,
    /// Gap width left uncut, mm.
    pub width: f64,
    /// Max perimeter distance between adjacent tabs, mm.
    pub max_spacing: f64,
    /// Minimum tabs on each outer contour.
    pub min_per_contour: u32,
    /// Keep tabs at least this far from segment corners, mm.
    pub corner_clearance: f64,
    /// Holes whose bbox min-dimension is below this get no tabs, mm.
    pub min_hole_size: f64,
}

impl Default for TabConfig {
    fn default() -> Self {
        TabConfig {
            enabled: false,
            width: 0.3,
            max_spacing: 250.0,
            min_per_contour: 2,
            corner_clearance: 3.0,
            min_hole_size: 40.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct NestConfig {
    /// Usable sheet length along X, mm. Ignored when `auto_width` is set.
    pub sheet_width: f64,
    /// Treat the stock as a strip of unbounded length: the nester is free to
    /// use as much X as it likes and the fitness function (which already
    /// minimises consumed length) decides how much that is. `sheet_width` is
    /// replaced by a bound wide enough to hold every part in a single row, so
    /// a second sheet is never opened and `stats.used_width` is the answer the
    /// operator actually wants — how far along the coil to cut.
    #[serde(default)]
    pub auto_width: bool,
    pub sheet_height: f64,
    /// Minimum gap between parts, mm.
    pub spacing: f64,
    /// Minimum gap between parts and the sheet edge, mm.
    pub margin: f64,
    pub rotation_mode: RotationMode,
    pub rotation_step_deg: f64,
    /// Max chord error when linearizing curves for nesting math, mm.
    pub curve_tolerance: f64,
    pub time_limit_ms: u64,
    pub population: usize,
    /// 0.0 - 1.0
    pub mutation_rate: f64,
    /// Consecutive generations without improvement before the GA gives up
    /// early (`time_limit_ms` is the hard stop either way).
    ///
    /// Swept on a real 13-instance job under `auto_width`, 3 runs each on a
    /// 20s budget (mean consumed width): 15 -> 5460mm finishing in 8-13s,
    /// 60 -> 5309mm, 150 -> 5229mm, 300 -> 5048mm, 600 -> 5066mm, 1200 ->
    /// 5277mm, 3000 -> 5206mm. Everything from 300 up is one plateau; the
    /// apparent regression past 600 is run-to-run noise, which at ~500mm
    /// spread is larger than the effect of this knob. 600 sits in the
    /// plateau and spends 15-20s of the budget.
    ///
    /// Raising this trades latency for layout quality, so it is a product
    /// call rather than a tuning constant — do not lower it to make jobs feel
    /// faster. Note it does nothing at all when the sheet width is pinned and
    /// the layout overflows: the fitness landscape is flat there, and no
    /// amount of extra generations helps.
    pub stale_generations: u32,
    /// Seed for the GA's random number generator. With a seed, the same
    /// input produces the same layout whenever the run ends on
    /// `stale_generations` rather than on the wall-clock limit (a time-based
    /// stop cuts the search at a machine-dependent generation). `None`
    /// draws a fresh seed per run.
    pub seed: Option<u64>,
    pub tabs: TabConfig,
}

impl Default for NestConfig {
    fn default() -> Self {
        NestConfig {
            sheet_width: 1829.0,
            auto_width: false,
            sheet_height: 914.0,
            spacing: 2.0,
            margin: 5.0,
            rotation_mode: RotationMode::Orthogonal,
            rotation_step_deg: 15.0,
            curve_tolerance: 0.25,
            time_limit_ms: 20_000,
            population: 15,
            mutation_rate: 0.10,
            stale_generations: 600,
            seed: None,
            tabs: TabConfig::default(),
        }
    }
}

impl NestConfig {
    /// The rotation angles the optimizer may choose from.
    pub fn allowed_rotations(&self) -> Vec<f64> {
        match self.rotation_mode {
            RotationMode::Orthogonal => vec![0.0, 90.0, 180.0, 270.0],
            RotationMode::Free => {
                let step = self.rotation_step_deg.clamp(1.0, 90.0);
                let n = (360.0 / step).round() as usize;
                (0..n).map(|i| i as f64 * step).collect()
            }
        }
    }
}

/// Why the optimizer stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// `time_limit_ms` elapsed.
    TimeLimit,
    /// `stale_generations` passed without improvement.
    Stale,
    /// The caller's `should_stop` hook returned true.
    Cancelled,
    /// There was nothing to place.
    Empty,
}

/// Snapshot handed to the progress hook once per generation.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    pub generation: u32,
    /// Fitness of the best individual so far (lower is better; it is the
    /// strip area consumed, mm², plus penalties).
    pub best_fitness: f64,
    /// Utilization of the best layout so far; measured against the stock
    /// actually consumed under `auto_width`, against the configured sheets
    /// otherwise.
    pub best_utilization: f64,
    pub elapsed_ms: u64,
}

/// One placed part instance in the result.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Placement {
    /// Index of the part in the slice passed to `nest`.
    pub part_id: usize,
    pub part_name: String,
    pub instance: u32,
    pub sheet: usize,
    pub rotation_deg: f64,
    pub dx: f64,
    pub dy: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NestStats {
    pub stop_reason: StopReason,
    pub sheets_used: usize,
    /// Length of stock actually consumed along X, mm: full sheets plus the
    /// used part of the last one. This is what the nester minimises, so it is
    /// the honest headline metric — `utilization` below is an area ratio that
    /// moves for reasons the optimizer does not care about.
    pub used_width: f64,
    /// net part area / (sheets_used * sheet area)
    pub utilization: f64,
    /// net part area / used bounding strip area of the last sheet + full
    /// area of the other sheets. More flattering, matches SVGnest's metric.
    pub strip_utilization: f64,
    pub generations: u32,
    pub elapsed_ms: u64,
    pub placed: usize,
    pub total: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NestSolution {
    pub placements: Vec<Placement>,
    pub stats: NestStats,
    /// Width of the sheet the placements are laid out on, mm. Equals the
    /// configured `sheet_width`, except under `auto_width` where it is the
    /// length the layout actually reached plus one margin — the width to
    /// draw the sheet at and to cut the coil to.
    pub sheet_width: f64,
    pub sheet_height: f64,
    pub warnings: Vec<String>,
}

impl NestSolution {
    /// The config to render this solution with: the run config, with the
    /// sheet width replaced by [`NestSolution::sheet_width`].
    pub fn render_config(&self, cfg: &NestConfig) -> NestConfig {
        NestConfig {
            sheet_width: self.sheet_width,
            sheet_height: self.sheet_height,
            ..cfg.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sq(x0: f64, y0: f64, s: f64) -> Vec<Pt> {
        vec![
            Pt::new(x0, y0),
            Pt::new(x0 + s, y0),
            Pt::new(x0 + s, y0 + s),
            Pt::new(x0, y0 + s),
        ]
    }

    #[test]
    fn from_polygon_normalizes_winding_origin_and_areas() {
        // Outer given clockwise and away from the origin, with a closing
        // duplicate; hole given counter-clockwise.
        let mut outer = sq(50.0, 20.0, 100.0);
        outer.reverse();
        outer.push(outer[0]);
        let hole = sq(60.0, 30.0, 10.0);
        let p = Part::from_polygon("plate", 3, &outer, &[hole]).unwrap();

        assert_eq!(p.name, "plate");
        assert_eq!(p.quantity, 3);
        assert!(p.outer.signed_area(0.25) > 0.0, "outer must be CCW");
        assert!(p.holes[0].signed_area(0.25) < 0.0, "hole must be CW");
        let (minx, miny, maxx, maxy) = ring_bbox(&p.outer_poly);
        assert!(minx.abs() < 1e-9 && miny.abs() < 1e-9);
        assert!((maxx - 100.0).abs() < 1e-9 && (maxy - 100.0).abs() < 1e-9);
        // The hole moved with the outer.
        let (hx, hy, _, _) = ring_bbox(&p.holes[0].polyline(0.25));
        assert!((hx - 10.0).abs() < 1e-9 && (hy - 10.0).abs() < 1e-9);
        assert!((p.gross_area - 10_000.0).abs() < 1e-9);
        assert!((p.net_area - 9_900.0).abs() < 1e-9);
        assert_eq!(p.outer.segs.len(), 4, "closing duplicate dropped");
    }

    #[test]
    fn from_polygon_rejects_degenerate_rings() {
        assert!(
            Part::from_polygon("line", 1, &[Pt::new(0.0, 0.0), Pt::new(5.0, 0.0)], &[]).is_err()
        );
        let flat = [Pt::new(0.0, 0.0), Pt::new(5.0, 0.0), Pt::new(10.0, 0.0)];
        assert!(Part::from_polygon("flat", 1, &flat, &[]).is_err());
        assert!(Part::from_polygon("nan", 1, &[Pt::new(f64::NAN, 0.0); 3], &[]).is_err());
        let bad_hole = vec![Pt::new(1.0, 1.0), Pt::new(2.0, 2.0)];
        assert!(Part::from_polygon("hole", 1, &sq(0.0, 0.0, 10.0), &[bad_hole]).is_err());
    }

    #[test]
    fn config_json_is_camel_case_and_round_trips() {
        let cfg = NestConfig {
            auto_width: true,
            seed: Some(9),
            ..NestConfig::default()
        };
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(json.contains("\"sheetWidth\""));
        assert!(json.contains("\"rotationMode\":\"orthogonal\""));
        assert!(json.contains("\"seed\":9"));
        let back: NestConfig = serde_json::from_str(&json).unwrap();
        assert!(back.auto_width);
        assert_eq!(back.seed, Some(9));
        // Missing fields fall back to defaults.
        let partial: NestConfig = serde_json::from_str(r#"{"sheetWidth": 1000}"#).unwrap();
        assert_eq!(partial.sheet_width, 1000.0);
        assert_eq!(partial.stale_generations, 600);
        assert_eq!(partial.seed, None);
    }
}
