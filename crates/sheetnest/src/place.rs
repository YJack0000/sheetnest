//! NFP-based placement: given an ordered list of part instances with chosen
//! rotations, greedily place them onto sheets (bottom-left / gravity-left
//! strategy). This is the GA's fitness decoder.
//!
//! Spacing semantics: NFPs are computed between A's raw polygon and B's
//! polygon inflated by `spacing`, giving a minimum part-to-part gap of
//! `spacing`. The sheet rectangle is shrunk by `margin` on all sides for
//! the inner-fit computation on raw polygons, giving a minimum part-to-edge
//! gap of `margin`.

use crate::geom;
use crate::model::{NestConfig, Part, Pt, ring_bbox};
use clipper2::Paths;
use dashmap::DashMap;
use std::sync::Arc;

/// Quantize a rotation angle to a hashable cache-key bucket.
fn rot_bucket(deg: f64) -> i64 {
    (deg * 1000.0).round() as i64
}

/// Cache of part-pair NFPs. Key: (stationary part id, stationary rotation
/// bucket, orbiting part id, orbiting rotation bucket). Rotation bucket =
/// (deg * 1000).round() as i64 to make angles hashable.
#[derive(Default)]
pub struct NfpCache {
    map: DashMap<(usize, i64, usize, i64), Arc<Paths>>,
}

impl NfpCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// NFP of orbiting part `b` at rotation `rot_b` around stationary part
    /// `a` at rotation `rot_a`, both in local frames (translation-free).
    /// `a` uses its raw outer polygon, `b` uses its spacing-inflated one.
    /// Computes and caches on miss.
    pub fn get(&self, a: &PreppedPart, rot_a: f64, b: &PreppedPart, rot_b: f64) -> Arc<Paths> {
        let key = (a.id, rot_bucket(rot_a), b.id, rot_bucket(rot_b));
        if let Some(hit) = self.map.get(&key) {
            return hit.value().clone();
        }
        // Rotate the cached local rings; never bake translations into the
        // cached NFP — callers translate the result per placed instance.
        let ra = geom::rotate_ring(&a.raw, rot_a);
        let rb = geom::rotate_ring(&b.inflated, rot_b);
        let computed = Arc::new(geom::nfp(&ra, &rb));
        // If a sibling thread raced us here, keep the first insert.
        self.map.entry(key).or_insert(computed).value().clone()
    }
}

/// Per-part precomputed geometry for one nesting run.
pub struct PreppedPart {
    pub id: usize,
    /// linearized outer ring, local frame.
    pub raw: Vec<Pt>,
    /// raw inflated by cfg.spacing.
    pub inflated: Vec<Pt>,
    pub gross_area: f64,
    pub net_area: f64,
}

/// Hard cap on ring vertices used for NFP math. The Minkowski sweep is
/// O(n·m) in vertices; spline-heavy real parts arrive with hundreds of
/// sample points and would blow the GA's generation time into minutes.
const MAX_NFP_POINTS: usize = 200;

/// Build `PreppedPart`s once per job.
///
/// Placement math runs on rings simplified at `curve_tolerance` (adaptively
/// coarser, up to a few mm, if a ring still exceeds `MAX_NFP_POINTS`).
/// Consequence: spacing/margin are honored within roughly ±2× the simplify
/// epsilon — with the default spacing 2mm / tolerance 0.25mm that leaves a
/// guaranteed gap ≥ 1.5mm. Stats and DXF/SVG output still use the exact
/// geometry.
pub fn prep_parts(parts: &[Part], cfg: &NestConfig) -> Vec<PreppedPart> {
    parts
        .iter()
        .enumerate()
        .map(|(id, p)| {
            let raw_full = if p.outer_poly.len() >= 3 {
                p.outer_poly.clone()
            } else {
                p.outer.polyline(cfg.curve_tolerance)
            };
            let mut eps = cfg.curve_tolerance.max(0.05);
            let mut raw = geom::simplify_ring(&raw_full, eps);
            while raw.len() > MAX_NFP_POINTS && eps < 8.0 {
                eps *= 2.0;
                raw = geom::simplify_ring(&raw_full, eps);
            }
            let inflated = geom::inflate_ring(&raw, cfg.spacing);
            PreppedPart {
                id,
                raw,
                inflated,
                gross_area: p.gross_area,
                net_area: p.net_area,
            }
        })
        .collect()
}

/// One gene of an individual: which instance goes next and at which of the
/// allowed rotation angles (index into NestConfig::allowed_rotations()).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Gene {
    /// index into the flattened instance list (see ga.rs).
    pub instance: usize,
    /// index into allowed_rotations().
    pub rotation: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlacedInstance {
    pub instance: usize,
    pub sheet: usize,
    pub rotation_deg: f64,
    pub dx: f64,
    pub dy: f64,
}

pub struct DecodeResult {
    pub placed: Vec<PlacedInstance>,
    /// instances that fit on no sheet at any position (should be empty
    /// unless a part is bigger than the sheet).
    pub unplaceable: Vec<usize>,
    /// Lower is better. Primary: number of sheets. Secondary: bounding
    /// strip width on the last sheet. Concretely:
    /// fitness = (sheets_used - 1) * sheet_area + last_sheet_max_x * sheet_height
    /// plus a large penalty per unplaceable instance.
    pub fitness: f64,
}

/// A part instance already placed on a sheet, decode-internal bookkeeping.
struct OnSheet {
    /// index into the `parts` slice (NOT the part id).
    part: usize,
    rot_deg: f64,
    dx: f64,
    dy: f64,
    /// dx + rotated-raw bbox maxx (world-frame right edge).
    world_maxx: f64,
}

/// Decode an ordering+rotations into concrete placements.
///
/// Greedy per instance, in gene order:
/// - try the gene's rotation first; if the part cannot fit anywhere with
///   it, fall back to trying the other allowed rotations before declaring
///   the instance unplaceable on this sheet;
/// - valid positions come from geom::candidate_positions(ifp_rect,
///   translated NFPs of everything already on the current sheet);
/// - among candidates pick the lexicographic min of
///   (resulting used-strip max_x, x, y) — gravity to the left wall;
/// - when nothing fits on the current sheet, open a new sheet (sheets are
///   unbounded; fitness punishes extra sheets).
///
/// `instance_part` maps instance index -> index into `parts`.
pub fn decode(
    genes: &[Gene],
    parts: &[PreppedPart],
    instance_part: &[usize],
    cfg: &NestConfig,
    cache: &NfpCache,
) -> DecodeResult {
    let rotations = cfg.allowed_rotations();
    let sheet_area = cfg.sheet_width * cfg.sheet_height;

    let mut sheets: Vec<Vec<OnSheet>> = Vec::new();
    let mut placed: Vec<PlacedInstance> = Vec::new();
    let mut unplaceable: Vec<usize> = Vec::new();

    // Rotated-raw bbox memo per (part index, rotation index).
    let mut bbox_memo: std::collections::HashMap<(usize, usize), (f64, f64, f64, f64)> =
        std::collections::HashMap::new();

    for gene in genes {
        let pi = instance_part[gene.instance];
        let part = &parts[pi];
        // Gene's rotation first, then the remaining allowed rotations in
        // listed order.
        let gr = gene.rotation.min(rotations.len().saturating_sub(1));
        let rot_order: Vec<usize> = std::iter::once(gr)
            .chain((0..rotations.len()).filter(|&k| k != gr))
            .collect();

        // (sheet index, rotation index, dx, dy, world_maxx)
        let mut chosen: Option<(usize, usize, f64, f64, f64)> = None;

        // Try existing sheets in order, then one fresh (empty) sheet.
        'sheet_loop: for si in 0..=sheets.len() {
            let is_new = si == sheets.len();
            for &ri in &rot_order {
                let rot_deg = rotations[ri];
                let (minx, miny, maxx, maxy) = *bbox_memo
                    .entry((pi, ri))
                    .or_insert_with(|| ring_bbox(&geom::rotate_ring(&part.raw, rot_deg)));
                // Inner-fit rectangle for the translation of this rotation.
                let tx0 = cfg.margin - minx;
                let tx1 = cfg.sheet_width - cfg.margin - maxx;
                let ty0 = cfg.margin - miny;
                let ty1 = cfg.sheet_height - cfg.margin - maxy;
                if tx1 - tx0 < -1e-9 || ty1 - ty0 < -1e-9 {
                    // This rotation cannot fit on ANY sheet; try the next.
                    continue;
                }
                let forbidden: Vec<Paths> = if is_new {
                    Vec::new()
                } else {
                    sheets[si]
                        .iter()
                        .map(|q| {
                            let nfp = cache.get(&parts[q.part], q.rot_deg, part, rot_deg);
                            geom::translate_paths(&nfp, q.dx, q.dy)
                        })
                        .collect()
                };
                let cands = geom::candidate_positions((tx0, ty0, tx1, ty1), &forbidden);
                if cands.is_empty() {
                    continue;
                }
                let cur_maxx = if is_new {
                    f64::NEG_INFINITY
                } else {
                    sheets[si]
                        .iter()
                        .map(|q| q.world_maxx)
                        .fold(f64::NEG_INFINITY, f64::max)
                };
                // Lexicographic min of (resulting strip max_x, x, y).
                let mut best: Option<(f64, f64, f64)> = None;
                for c in &cands {
                    let strip = cur_maxx.max(c.x + maxx);
                    let key = (strip, c.x, c.y);
                    if best.is_none_or(|b| key < b) {
                        best = Some(key);
                    }
                }
                let (_, bx, by) = best.unwrap();
                chosen = Some((si, ri, bx, by, bx + maxx));
                break 'sheet_loop;
            }
        }

        match chosen {
            Some((si, ri, dx, dy, world_maxx)) => {
                if si == sheets.len() {
                    sheets.push(Vec::new());
                }
                let rot_deg = rotations[ri];
                sheets[si].push(OnSheet {
                    part: pi,
                    rot_deg,
                    dx,
                    dy,
                    world_maxx,
                });
                placed.push(PlacedInstance {
                    instance: gene.instance,
                    sheet: si,
                    rotation_deg: rot_deg,
                    dx,
                    dy,
                });
            }
            None => unplaceable.push(gene.instance),
        }
    }

    let sheets_used = sheets.len();
    let mut fitness = unplaceable.len() as f64 * 2.0 * sheet_area;
    if sheets_used > 0 {
        let last_max_x = sheets
            .last()
            .unwrap()
            .iter()
            .map(|q| q.world_maxx)
            .fold(0.0_f64, f64::max);
        fitness += (sheets_used - 1) as f64 * sheet_area + last_max_x * cfg.sheet_height;
    }

    DecodeResult {
        placed,
        unplaceable,
        fitness,
    }
}

/// Final overlap sanity check on a finished layout: returns pairs of
/// instances whose raw polygons overlap by more than `eps_area` (should be
/// none; used before returning results to the API and in tests).
pub fn verify_no_overlaps(
    placed: &[PlacedInstance],
    parts: &[PreppedPart],
    instance_part: &[usize],
    eps_area: f64,
) -> Vec<(usize, usize)> {
    let rings: Vec<Vec<Pt>> = placed
        .iter()
        .map(|p| {
            let raw = &parts[instance_part[p.instance]].raw;
            geom::translate_ring(&geom::rotate_ring(raw, p.rotation_deg), p.dx, p.dy)
        })
        .collect();
    let mut out = Vec::new();
    for i in 0..placed.len() {
        for j in (i + 1)..placed.len() {
            if placed[i].sheet != placed[j].sheet {
                continue;
            }
            if geom::overlap_area(&rings[i], &rings[j]) > eps_area {
                out.push((placed[i].instance, placed[j].instance));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    // Required tests (implementer: keep these, add more):
    // - two 10x10 squares on a 25x12 sheet (margin 1, spacing 2): both must
    //   land on sheet 0, no overlap, gap >= 2 (check via overlap of
    //   spacing/2-inflated rings), everything within margins.
    // - four 10x10 squares on a 12x12 sheet: 4 sheets used.
    // - a part larger than the sheet -> unplaceable, fitness penalized.
    // - decode is deterministic for a fixed gene sequence.
    use super::*;
    use crate::model::{Contour, Seg};

    fn rect_part(id: usize, w: f64, h: f64, qty: u32) -> Part {
        let pts = [
            Pt::new(0.0, 0.0),
            Pt::new(w, 0.0),
            Pt::new(w, h),
            Pt::new(0.0, h),
        ];
        let segs = (0..4)
            .map(|i| Seg::Line {
                a: pts[i],
                b: pts[(i + 1) % 4],
            })
            .collect();
        let outer = Contour { segs };
        let outer_poly = outer.polyline(0.25);
        Part {
            name: format!("p{id}"),
            quantity: qty,
            outer,
            holes: vec![],
            outer_poly,
            gross_area: w * h,
            net_area: w * h,
        }
    }

    fn cfg(w: f64, h: f64, spacing: f64, margin: f64) -> NestConfig {
        NestConfig {
            sheet_width: w,
            sheet_height: h,
            spacing,
            margin,
            ..NestConfig::default()
        }
    }

    fn genes(n: usize) -> Vec<Gene> {
        (0..n)
            .map(|i| Gene {
                instance: i,
                rotation: 0,
            })
            .collect()
    }

    fn transform(ring: &[Pt], deg: f64, dx: f64, dy: f64) -> Vec<Pt> {
        geom::translate_ring(&geom::rotate_ring(ring, deg), dx, dy)
    }

    #[test]
    fn prep_parts_inflates_by_spacing() {
        let c = cfg(100.0, 100.0, 2.0, 1.0);
        let prepped = prep_parts(&[rect_part(0, 10.0, 10.0, 1)], &c);
        assert_eq!(prepped.len(), 1);
        let (rminx, rminy, rmaxx, rmaxy) = ring_bbox(&prepped[0].raw);
        assert!((rmaxx - rminx - 10.0).abs() < 1e-6);
        assert!((rmaxy - rminy - 10.0).abs() < 1e-6);
        let (iminx, iminy, imaxx, imaxy) = ring_bbox(&prepped[0].inflated);
        assert!((imaxx - iminx - 14.0).abs() < 1e-6);
        assert!((imaxy - iminy - 14.0).abs() < 1e-6);
    }

    #[test]
    fn two_squares_one_sheet_gap_and_margins() {
        let c = cfg(25.0, 12.0, 2.0, 1.0);
        let parts = vec![rect_part(0, 10.0, 10.0, 1), rect_part(1, 10.0, 10.0, 1)];
        let prepped = prep_parts(&parts, &c);
        let instance_part = [0usize, 1];
        let cache = NfpCache::new();
        let res = decode(&genes(2), &prepped, &instance_part, &c, &cache);

        assert!(res.unplaceable.is_empty());
        assert_eq!(res.placed.len(), 2);
        assert!(res.placed.iter().all(|p| p.sheet == 0), "both on sheet 0");
        assert!(
            verify_no_overlaps(&res.placed, &prepped, &instance_part, 0.01).is_empty(),
            "raw rings must not overlap"
        );

        // gap >= spacing (2mm): rings inflated by spacing/2 must not overlap.
        let half: Vec<Vec<Pt>> = res
            .placed
            .iter()
            .map(|p| {
                let inf = geom::inflate_ring(&prepped[instance_part[p.instance]].raw, 1.0);
                transform(&inf, p.rotation_deg, p.dx, p.dy)
            })
            .collect();
        assert!(geom::overlap_area(&half[0], &half[1]) < 1e-6);

        // everything within margins.
        for p in &res.placed {
            let ring = transform(
                &prepped[instance_part[p.instance]].raw,
                p.rotation_deg,
                p.dx,
                p.dy,
            );
            let (minx, miny, maxx, maxy) = ring_bbox(&ring);
            assert!(
                minx >= 1.0 - 1e-6 && maxx <= 24.0 + 1e-6,
                "x within margins: {minx}..{maxx}"
            );
            assert!(
                miny >= 1.0 - 1e-6 && maxy <= 11.0 + 1e-6,
                "y within margins: {miny}..{maxy}"
            );
        }
    }

    #[test]
    fn four_squares_four_sheets() {
        // Only one 10x10 square fits per 12x12 sheet (margin 0.5, spacing 2).
        let c = cfg(12.0, 12.0, 2.0, 0.5);
        let parts = vec![rect_part(0, 10.0, 10.0, 4)];
        let prepped = prep_parts(&parts, &c);
        let instance_part = [0usize, 0, 0, 0];
        let cache = NfpCache::new();
        let res = decode(&genes(4), &prepped, &instance_part, &c, &cache);

        assert!(res.unplaceable.is_empty());
        assert_eq!(res.placed.len(), 4);
        let mut sheets: Vec<usize> = res.placed.iter().map(|p| p.sheet).collect();
        sheets.sort_unstable();
        assert_eq!(sheets, vec![0, 1, 2, 3], "one square per sheet");
        // fitness reflects 4 sheets: at least 3 full sheets' area.
        assert!(res.fitness > 3.0 * 144.0 - 1e-6);
    }

    #[test]
    fn oversized_part_is_unplaceable_and_penalized() {
        let c = cfg(12.0, 12.0, 2.0, 0.5);
        let parts = vec![rect_part(0, 30.0, 30.0, 1)];
        let prepped = prep_parts(&parts, &c);
        let instance_part = [0usize];
        let cache = NfpCache::new();
        let res = decode(&genes(1), &prepped, &instance_part, &c, &cache);

        assert!(res.placed.is_empty());
        assert_eq!(res.unplaceable, vec![0]);
        // penalty = 2 * sheet_area per unplaceable instance.
        assert!((res.fitness - 2.0 * 144.0).abs() < 1e-6);
    }

    #[test]
    fn decode_is_deterministic() {
        let c = cfg(200.0, 200.0, 2.0, 5.0);
        let parts = vec![
            rect_part(0, 40.0, 30.0, 1),
            rect_part(1, 100.0, 20.0, 1),
            rect_part(2, 60.0, 60.0, 1),
        ];
        let prepped = prep_parts(&parts, &c);
        let instance_part = [0usize, 1, 2];
        let g = vec![
            Gene {
                instance: 2,
                rotation: 1,
            },
            Gene {
                instance: 0,
                rotation: 3,
            },
            Gene {
                instance: 1,
                rotation: 0,
            },
        ];
        let cache1 = NfpCache::new();
        let r1 = decode(&g, &prepped, &instance_part, &c, &cache1);
        let cache2 = NfpCache::new();
        let r2 = decode(&g, &prepped, &instance_part, &c, &cache2);
        assert_eq!(r1.placed, r2.placed);
        assert_eq!(r1.unplaceable, r2.unplaceable);
        assert_eq!(r1.fitness.to_bits(), r2.fitness.to_bits());
    }

    #[test]
    fn gene_rotation_falls_back_when_it_cannot_fit() {
        // 100x10 bar on a 45x120 sheet: rotation 0 cannot fit (too wide),
        // decode must fall back to a 90-degree rotation.
        let c = cfg(45.0, 120.0, 0.5, 1.0);
        let parts = vec![rect_part(0, 100.0, 10.0, 1)];
        let prepped = prep_parts(&parts, &c);
        let instance_part = [0usize];
        let cache = NfpCache::new();
        let res = decode(&genes(1), &prepped, &instance_part, &c, &cache);
        assert_eq!(res.placed.len(), 1);
        assert!((res.placed[0].rotation_deg - 90.0).abs() < 1e-9);
    }

    #[test]
    fn first_instance_lands_bottom_left() {
        let c = cfg(100.0, 50.0, 2.0, 5.0);
        let parts = vec![rect_part(0, 10.0, 10.0, 1)];
        let prepped = prep_parts(&parts, &c);
        let cache = NfpCache::new();
        let res = decode(&genes(1), &prepped, &[0], &c, &cache);
        assert_eq!(res.placed.len(), 1);
        assert!((res.placed[0].dx - 5.0).abs() < 1e-9);
        assert!((res.placed[0].dy - 5.0).abs() < 1e-9);
    }
}
