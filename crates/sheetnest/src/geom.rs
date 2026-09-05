//! Polygon math on top of clipper2. This module is the single place that
//! touches clipper2 types; the rest of the codebase works with `Vec<Pt>`.
//!
//! All rings follow model.rs conventions: mm units, closed, no repeated
//! last point.
//!
//! Implementation notes:
//! - clipper2's default `PointScaler` is `Centi` (integer grid = 0.01 mm),
//!   which is an acceptable resolution for sheet cutting. All public APIs
//!   stay in f64 mm; scaling happens inside clipper2.
//! - clipper2's `inflate` wrapper multiplies *every* numeric argument by the
//!   point scaler, including the (dimensionless) miter limit, so we
//!   pre-divide the miter limit by the multiplier to hand the C++ core the
//!   intended value.

use crate::model::{Pt, ring_signed_area};
use clipper2::*;

/// Dedup tolerance for candidate positions, mm.
const DEDUP_TOL: f64 = 1e-6;

/// Tolerance used when checking that a harvested vertex lies inside/on the
/// IFP rectangle. clipper2 quantizes coordinates to a 0.01 mm grid, so
/// vertices of a boolean result can sit up to half a grid step (0.005 mm)
/// outside the exact rectangle; anything within 0.006 mm is accepted.
const RECT_TOL: f64 = 6e-3;

/// NFP hole rings with less translation area than this are dropped as
/// quantization pinholes (see the guard inside [`nfp`]).
const MIN_NFP_HOLE_AREA: f64 = 1.0;

/// Convert a ring to a clipper2 `Paths` (one closed path).
pub fn ring_to_paths(ring: &[Pt]) -> Paths {
    Paths::from(ring_to_tuples(ring))
}

/// Convert clipper2 `Paths` back into rings.
pub fn paths_to_rings(paths: &Paths) -> Vec<Vec<Pt>> {
    paths
        .iter()
        .map(|path| path.iter().map(|p| Pt::new(p.x(), p.y())).collect())
        .collect()
}

/// Inflate (offset) a closed polygon outward by `delta` mm (delta may be 0,
/// which must return the input unchanged). Returns only the largest-area
/// resulting ring (parts are simple polygons; offsetting can create slivers
/// which we discard). Join style: Miter with a sane miter limit, falling
/// back to clipper2 defaults.
pub fn inflate_ring(ring: &[Pt], delta: f64) -> Vec<Pt> {
    if delta == 0.0 || ring.len() < 3 {
        return ring.to_vec();
    }

    // clipper2 inflates positively-wound rings outward; normalize to CCW and
    // restore the caller's orientation on the way out.
    let was_cw = ring_signed_area(ring) < 0.0;
    let mut work: Vec<Pt> = ring.to_vec();
    if was_cw {
        work.reverse();
    }

    // Effective miter limit 2.0 (Clipper2's default). The Rust wrapper
    // scales this argument by the point multiplier, hence the division.
    let miter_limit = 2.0 / <Centi as PointScaler>::MULTIPLIER;
    let inflated = ring_to_paths(&work)
        .inflate(delta, JoinType::Miter, EndType::Polygon, miter_limit)
        .simplify(1e-3, false);

    let mut best_area = f64::NEG_INFINITY;
    let mut best: Vec<Pt> = Vec::new();
    for path in inflated.iter() {
        let area = path.signed_area().abs();
        if area > best_area {
            best_area = area;
            best = path.iter().map(|p| Pt::new(p.x(), p.y())).collect();
        }
    }
    if was_cw {
        best.reverse();
    }
    best
}

/// Ramer-Douglas-Peucker-style simplification of a closed ring (clipper2's
/// SimplifyPaths): removes vertices whose perpendicular deviation is under
/// `eps` mm. Used to keep NFP cost sane on spline-heavy inputs — the
/// Minkowski sweep is O(n·m) in ring vertices. Preserves the caller's ring
/// orientation. Returns the input unchanged when `eps <= 0` or the result
/// would degenerate below 3 points.
pub fn simplify_ring(ring: &[Pt], eps: f64) -> Vec<Pt> {
    if eps <= 0.0 || ring.len() < 4 {
        return ring.to_vec();
    }
    let was_cw = ring_signed_area(ring) < 0.0;
    let simplified = simplify(ring_to_paths(ring), eps, false);
    // Keep the largest ring; simplification of a simple polygon returns one.
    let mut best: Vec<Pt> = Vec::new();
    let mut best_area = f64::NEG_INFINITY;
    for path in simplified.iter() {
        let area = path.signed_area().abs();
        if area > best_area {
            best_area = area;
            best = path.iter().map(|p| Pt::new(p.x(), p.y())).collect();
        }
    }
    if best.len() < 3 {
        return ring.to_vec();
    }
    if was_cw != (ring_signed_area(&best) < 0.0) {
        best.reverse();
    }
    best
}

/// No-fit polygon of orbiting polygon B relative to stationary polygon A,
/// both defined about their own local origins.
///
/// Contract: translating B by any point t strictly inside the returned
/// region makes B overlap A; t on the boundary means touching contact;
/// t outside means no contact. Computed as MinkowskiSum(A, -B) where -B is
/// B reflected through the origin. The result may contain multiple rings
/// (outer boundary + holes where B fits inside concavities of A) — return
/// all of them as clipper2 `Paths` so the caller can do boolean ops.
pub fn nfp(a: &[Pt], b: &[Pt]) -> Paths {
    if a.len() < 3 || b.len() < 3 {
        return Paths::default();
    }

    // Orientation-normalize so the seed rings below carry positive winding.
    let mut a_ccw = a.to_vec();
    if ring_signed_area(&a_ccw) < 0.0 {
        a_ccw.reverse();
    }
    let mut b_ccw = b.to_vec();
    if ring_signed_area(&b_ccw) < 0.0 {
        b_ccw.reverse();
    }
    // Reflect B through the origin (a point reflection preserves winding).
    let neg_b: Vec<Pt> = b_ccw.iter().map(|p| Pt::new(-p.x, -p.y)).collect();

    // Boundary sweep: clipper2's Minkowski sum is the union of quads spanned
    // by consecutive translated copies of the pattern along the path, i.e.
    // exactly the set of translations where the two CONTOURS touch or cross.
    let pattern: Path = Path::from(ring_to_tuples(&neg_b));
    let sweep = ring_to_paths(&a_ccw).minkowski_sum(pattern, true);

    // The sweep alone leaves interior holes wherever one polygon strictly
    // contains the other — which IS overlap and must be filled. Two seed
    // regions (both provable subsets of the true filled Minkowski sum
    // A ⊕ -B) cover exactly those cases:
    //   A + (-b0)  covers every t with (B + t) strictly inside A,
    //   -B + a0    covers every t with A strictly inside (B + t).
    // Genuine concavity holes (B sits in a pocket of A with no contact)
    // intersect neither seed and survive as holes, as required.
    let seed_a = ring_to_paths(&translate_ring(&a_ccw, neg_b[0].x, neg_b[0].y));
    let seed_b = ring_to_paths(&translate_ring(&neg_b, a_ccw[0].x, a_ccw[0].y));

    let unioned = sweep
        .to_clipper_subject()
        .add_clip(seed_a)
        .add_clip(seed_b)
        .union(FillRule::NonZero)
        .unwrap_or(sweep);

    // Quantization guard: at non-axis-aligned rotations, clipper's 0.01mm
    // integer grid collapses near-duplicate vertices into degenerate sweep
    // quads, leaving microscopic "pinhole" hole rings INSIDE the forbidden
    // region (measured: 352 pinholes of 0.0005-0.045 mm^2 across 14400
    // NFPs). A candidate position harvested from such a hole overlaps the
    // stationary part almost fully. Drop hole rings below 1 mm^2 of
    // translation freedom — a genuine concavity pocket with less wiggle
    // than that is a tolerance-critical placement we refuse anyway.
    let filtered: Vec<Path> = unioned
        .iter()
        .filter(|path| {
            let ring: Vec<Pt> = path.iter().map(|p| Pt::new(p.x(), p.y())).collect();
            let area = ring_signed_area(&ring);
            area >= 0.0 || area.abs() >= MIN_NFP_HOLE_AREA
        })
        .cloned()
        .collect();
    Paths::new(filtered)
}

/// True if point p is strictly inside any ring of `paths` (boundary does
/// NOT count as inside).
pub fn point_strictly_inside(p: Pt, paths: &Paths) -> bool {
    let mut inside = 0usize;
    for path in paths.iter() {
        if path.is_empty() {
            continue;
        }
        match path.is_point_inside(Point::new(p.x, p.y)) {
            PointInPolygonResult::IsOn => return false,
            PointInPolygonResult::IsInside => inside += 1,
            PointInPolygonResult::IsOutside => {}
        }
    }
    // Even-odd containment: a point inside an NFP hole ring as well as the
    // outer ring is NOT inside the region.
    inside % 2 == 1
}

/// Compute the candidate positions for placing an orbiting part:
/// `ifp_rect` = (minx, miny, maxx, maxy) is the valid translation rectangle
/// from the sheet bounds; `forbidden` are translated NFPs of already-placed
/// parts. Valid region = rect minus union(forbidden).
///
/// Returns the vertices of the valid region's boundary (dedup within 1e-6).
/// If the valid region is empty, returns an empty vec. Every returned point
/// must satisfy: inside/on the rect, and NOT strictly inside any forbidden
/// region (touching boundaries is allowed and expected — that is where
/// optimal packings live).
pub fn candidate_positions(ifp_rect: (f64, f64, f64, f64), forbidden: &[Paths]) -> Vec<Pt> {
    let (minx, miny, maxx, maxy) = ifp_rect;
    if maxx - minx < -1e-9 || maxy - miny < -1e-9 {
        return Vec::new();
    }
    let corners = [
        Pt::new(minx, miny),
        Pt::new(maxx, miny),
        Pt::new(maxx, maxy),
        Pt::new(minx, maxy),
    ];

    let has_forbidden = forbidden.iter().any(|f| f.iter().any(|p| p.len() >= 3));
    // A zero-width/height rect has no area for clipper to work with; fall
    // back to its (deduped) corners so exact-fit parts remain placeable.
    let degenerate = (maxx - minx) < 1e-9 || (maxy - miny) < 1e-9;

    let mut raw: Vec<Pt> = Vec::new();
    if has_forbidden && !degenerate {
        // union(forbidden): rings coming out of nfp() are properly oriented
        // (outers positive, holes negative), so a NonZero union of all rings
        // is the correct set-union of the individual even-odd regions.
        let mut merged_src = Paths::default();
        for f in forbidden {
            merged_src.append(f.clone());
        }
        let merged = match merged_src
            .to_clipper_subject()
            .add_clip(Paths::default())
            .union(FillRule::NonZero)
        {
            Ok(m) => m,
            Err(_) => return Vec::new(), // conservative: no candidates
        };
        let valid = match ring_to_paths(&corners)
            .to_clipper_subject()
            .add_clip(merged)
            .difference(FillRule::NonZero)
        {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };
        // Harvest every vertex: outer rings AND hole rings — optimal
        // placements live on NFP boundaries.
        for ring in paths_to_rings(&valid) {
            raw.extend(ring);
        }
    }
    // The exact (unquantized) rect corners are always candidates when they
    // survive the filters below; this also covers the empty-forbidden and
    // degenerate-rect cases without a clipper round-trip.
    raw.extend(corners);

    let mut out = dedupe_points(raw, DEDUP_TOL);
    out.retain(|p| {
        p.x >= minx - RECT_TOL
            && p.x <= maxx + RECT_TOL
            && p.y >= miny - RECT_TOL
            && p.y <= maxy + RECT_TOL
            && !forbidden.iter().any(|f| point_strictly_inside(*p, f))
    });
    out
}

/// Area of the overlap between two closed rings (mm^2). Used by tests and
/// the final sanity check to guarantee no overlapping placements.
pub fn overlap_area(a: &[Pt], b: &[Pt]) -> f64 {
    if a.len() < 3 || b.len() < 3 {
        return 0.0;
    }
    ring_to_paths(a)
        .to_clipper_subject()
        .add_clip(ring_to_paths(b))
        .intersect(FillRule::NonZero)
        .map(|p| p.signed_area().abs())
        .unwrap_or(0.0)
}

/// Union of many rings, returned as rings (used for merged-extent stats).
pub fn union_rings(rings: &[Vec<Pt>]) -> Vec<Vec<Pt>> {
    let mut subject = Paths::default();
    for ring in rings {
        if ring.len() < 3 {
            continue;
        }
        // NonZero union needs consistent (positive) winding.
        if ring_signed_area(ring) < 0.0 {
            let mut r = ring.clone();
            r.reverse();
            subject.push(ring_to_paths(&r));
        } else {
            subject.push(ring_to_paths(ring));
        }
    }
    if subject.is_empty() {
        return Vec::new();
    }
    match subject
        .to_clipper_subject()
        .add_clip(Paths::default())
        .union(FillRule::NonZero)
    {
        Ok(u) => paths_to_rings(&u),
        Err(_) => rings.to_vec(), // best effort: return inputs unmerged
    }
}

pub fn rotate_ring(ring: &[Pt], deg: f64) -> Vec<Pt> {
    ring.iter().map(|p| p.rotated(deg)).collect()
}

pub fn translate_ring(ring: &[Pt], dx: f64, dy: f64) -> Vec<Pt> {
    ring.iter().map(|p| p.translated(dx, dy)).collect()
}

/// Translate every path in `paths` (helper over clipper2's translate).
pub fn translate_paths(paths: &Paths, dx: f64, dy: f64) -> Paths {
    paths.translate(dx, dy)
}

fn ring_to_tuples(ring: &[Pt]) -> Vec<(f64, f64)> {
    ring.iter().map(|p| (p.x, p.y)).collect()
}

/// Remove points that lie within `tol` of an already-kept point.
/// Grid-hash based so it stays O(n) for the sizes we see.
fn dedupe_points(points: Vec<Pt>, tol: f64) -> Vec<Pt> {
    use std::collections::HashMap;
    let inv = 1.0 / tol;
    let mut grid: HashMap<(i64, i64), Vec<Pt>> = HashMap::new();
    let mut out: Vec<Pt> = Vec::with_capacity(points.len());
    for p in points {
        if !p.x.is_finite() || !p.y.is_finite() {
            continue;
        }
        let cx = (p.x * inv).round() as i64;
        let cy = (p.y * inv).round() as i64;
        let mut dup = false;
        'scan: for dx in -1..=1i64 {
            for dy in -1..=1i64 {
                if let Some(bucket) = grid.get(&(cx + dx, cy + dy))
                    && bucket.iter().any(|q| p.dist(q) < tol)
                {
                    dup = true;
                    break 'scan;
                }
            }
        }
        if !dup {
            grid.entry((cx, cy)).or_default().push(p);
            out.push(p);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    // Required tests (implementer: keep these, add more):
    // - nfp of two unit squares gives outer ring spanning [-1,1]x[-1,1]
    // - candidate_positions with empty forbidden returns the 4 rect corners
    // - candidate_positions where forbidden fully covers rect returns empty
    // - overlap_area of disjoint rings is 0, of identical unit squares is 1
    // - inflate_ring of a 10x10 square by 1 has bbox 12x12
    use super::*;
    use crate::model::ring_bbox;

    fn square(x0: f64, y0: f64, w: f64, h: f64) -> Vec<Pt> {
        vec![
            Pt::new(x0, y0),
            Pt::new(x0 + w, y0),
            Pt::new(x0 + w, y0 + h),
            Pt::new(x0, y0 + h),
        ]
    }

    fn unit_square() -> Vec<Pt> {
        square(0.0, 0.0, 1.0, 1.0)
    }

    fn assert_close(a: f64, b: f64, tol: f64) {
        assert!((a - b).abs() <= tol, "expected {b} +- {tol}, got {a}");
    }

    fn largest_ring(paths: &Paths) -> Vec<Pt> {
        paths_to_rings(paths)
            .into_iter()
            .max_by(|r1, r2| {
                ring_signed_area(r1)
                    .abs()
                    .partial_cmp(&ring_signed_area(r2).abs())
                    .unwrap()
            })
            .expect("no rings")
    }

    fn contains_pt(points: &[Pt], p: Pt) -> bool {
        points.iter().any(|q| q.dist(&p) < 1e-6)
    }

    // ---- nfp ----------------------------------------------------------

    #[test]
    fn nfp_unit_squares_outer_spans_pm1() {
        let n = nfp(&unit_square(), &unit_square());
        assert!(!n.is_empty());
        let outer = largest_ring(&n);
        let (minx, miny, maxx, maxy) = ring_bbox(&outer);
        assert_close(minx, -1.0, 1e-6);
        assert_close(miny, -1.0, 1e-6);
        assert_close(maxx, 1.0, 1e-6);
        assert_close(maxy, 1.0, 1e-6);
        // Contract points: strictly inside => overlap.
        assert!(point_strictly_inside(Pt::new(0.0, 0.0), &n));
        assert!(point_strictly_inside(Pt::new(0.5, -0.5), &n));
        // Boundary => touching, not inside.
        assert!(!point_strictly_inside(Pt::new(1.0, 0.0), &n));
        assert!(!point_strictly_inside(Pt::new(-1.0, -1.0), &n));
        // Outside => no contact.
        assert!(!point_strictly_inside(Pt::new(1.5, 0.0), &n));
    }

    #[test]
    fn nfp_small_square_strictly_inside_big_square_is_forbidden() {
        // B strictly inside A is full overlap: the boundary-sweep hole must
        // be filled by the seed union.
        let n = nfp(&square(0.0, 0.0, 10.0, 10.0), &unit_square());
        let outer = largest_ring(&n);
        let (minx, miny, maxx, maxy) = ring_bbox(&outer);
        assert_close(minx, -1.0, 1e-6);
        assert_close(miny, -1.0, 1e-6);
        assert_close(maxx, 10.0, 1e-6);
        assert_close(maxy, 10.0, 1e-6);
        assert!(point_strictly_inside(Pt::new(4.0, 4.0), &n));
        assert!(point_strictly_inside(Pt::new(0.5, 5.0), &n));
        assert!(!point_strictly_inside(Pt::new(-1.0, 5.0), &n));
        // ... and the mirrored case: A strictly inside B.
        let m = nfp(&unit_square(), &square(0.0, 0.0, 10.0, 10.0));
        assert!(point_strictly_inside(Pt::new(-4.0, -4.0), &m));
    }

    #[test]
    fn nfp_concavity_hole_is_not_forbidden() {
        // C-shaped A: 10x10 square with a 4x4 cavity (x in [3,7], y in [2,6])
        // reachable only through a 0.5 mm wide channel from the top edge.
        // B (2x2) fits in the cavity but cannot pass the channel, so the NFP
        // must contain a hole around those translations.
        let a = vec![
            Pt::new(0.0, 0.0),
            Pt::new(10.0, 0.0),
            Pt::new(10.0, 10.0),
            Pt::new(5.25, 10.0),
            Pt::new(5.25, 6.0),
            Pt::new(7.0, 6.0),
            Pt::new(7.0, 2.0),
            Pt::new(3.0, 2.0),
            Pt::new(3.0, 6.0),
            Pt::new(4.75, 6.0),
            Pt::new(4.75, 10.0),
            Pt::new(0.0, 10.0),
        ];
        let b = square(0.0, 0.0, 2.0, 2.0);
        let n = nfp(&a, &b);
        assert!(n.len() >= 2, "expected outer ring + concavity hole");
        // B parked in the middle of the cavity: no contact => NOT forbidden.
        assert!(!point_strictly_inside(Pt::new(4.0, 3.0), &n));
        // B overlapping solid material => forbidden.
        assert!(point_strictly_inside(Pt::new(0.5, 0.5), &n));
        assert!(point_strictly_inside(Pt::new(8.5, 5.0), &n));
    }

    // ---- candidate_positions -----------------------------------------

    #[test]
    fn candidates_empty_forbidden_returns_rect_corners() {
        let got = candidate_positions((10.0, 20.0, 110.0, 70.0), &[]);
        assert_eq!(got.len(), 4);
        for c in [
            Pt::new(10.0, 20.0),
            Pt::new(110.0, 20.0),
            Pt::new(110.0, 70.0),
            Pt::new(10.0, 70.0),
        ] {
            assert!(contains_pt(&got, c), "missing corner {c:?}");
        }
    }

    #[test]
    fn candidates_fully_covered_rect_returns_empty() {
        let forbidden = vec![ring_to_paths(&square(-10.0, -10.0, 80.0, 60.0))];
        let got = candidate_positions((0.0, 0.0, 50.0, 30.0), &forbidden);
        assert!(got.is_empty(), "expected empty, got {got:?}");
    }

    #[test]
    fn candidates_partial_forbidden_lie_on_boundaries() {
        // rect [0,10]^2 minus forbidden [-2,5]^2 => L-shaped valid region.
        let forbidden = vec![ring_to_paths(&square(-2.0, -2.0, 7.0, 7.0))];
        let got = candidate_positions((0.0, 0.0, 10.0, 10.0), &forbidden);
        // The covered corner must be gone...
        assert!(!contains_pt(&got, Pt::new(0.0, 0.0)));
        // ...surviving rect corners kept...
        assert!(contains_pt(&got, Pt::new(10.0, 0.0)));
        assert!(contains_pt(&got, Pt::new(10.0, 10.0)));
        assert!(contains_pt(&got, Pt::new(0.0, 10.0)));
        // ...and the NFP-boundary vertices present (touching allowed).
        assert!(contains_pt(&got, Pt::new(5.0, 0.0)));
        assert!(contains_pt(&got, Pt::new(5.0, 5.0)));
        assert!(contains_pt(&got, Pt::new(0.0, 5.0)));
        // Every point honors the per-point contract.
        for p in &got {
            assert!(p.x >= -RECT_TOL && p.x <= 10.0 + RECT_TOL);
            assert!(p.y >= -RECT_TOL && p.y <= 10.0 + RECT_TOL);
            assert!(!forbidden.iter().any(|f| point_strictly_inside(*p, f)));
        }
    }

    #[test]
    fn candidates_inverted_rect_is_empty() {
        assert!(candidate_positions((5.0, 0.0, 3.0, 10.0), &[]).is_empty());
    }

    // ---- overlap_area -------------------------------------------------

    #[test]
    fn overlap_area_disjoint_is_zero_identical_is_one() {
        let a = unit_square();
        let far = square(5.0, 5.0, 1.0, 1.0);
        assert_close(overlap_area(&a, &far), 0.0, 1e-9);
        assert_close(overlap_area(&a, &a), 1.0, 1e-6);
        // Half overlap sanity.
        let shifted = square(0.5, 0.0, 1.0, 1.0);
        assert_close(overlap_area(&a, &shifted), 0.5, 1e-6);
    }

    // ---- inflate_ring -------------------------------------------------

    #[test]
    fn inflate_ring_square_grows_bbox_and_keeps_miter_corners() {
        let inflated = inflate_ring(&square(0.0, 0.0, 10.0, 10.0), 1.0);
        let (minx, miny, maxx, maxy) = ring_bbox(&inflated);
        assert_close(maxx - minx, 12.0, 1e-6);
        assert_close(maxy - miny, 12.0, 1e-6);
        // Sharp (mitered) corners => full 12x12 area, not chamfered.
        assert_close(ring_signed_area(&inflated).abs(), 144.0, 0.1);
    }

    #[test]
    fn inflate_ring_zero_delta_returns_input() {
        let ring = square(3.0, 4.0, 7.0, 2.0);
        assert_eq!(inflate_ring(&ring, 0.0), ring);
    }

    #[test]
    fn inflate_ring_preserves_input_orientation() {
        let mut cw = square(0.0, 0.0, 10.0, 10.0);
        cw.reverse(); // clockwise ring
        let inflated = inflate_ring(&cw, 1.0);
        assert!(ring_signed_area(&inflated) < 0.0);
        let (minx, miny, maxx, maxy) = ring_bbox(&inflated);
        assert_close(maxx - minx, 12.0, 1e-6);
        assert_close(maxy - miny, 12.0, 1e-6);
    }

    // ---- union / conversions / translate ------------------------------

    #[test]
    fn union_rings_merges_overlapping_squares() {
        let merged = union_rings(&[square(0.0, 0.0, 2.0, 2.0), square(1.0, 1.0, 2.0, 2.0)]);
        assert_eq!(merged.len(), 1);
        assert_close(ring_signed_area(&merged[0]).abs(), 7.0, 1e-6);
    }

    #[test]
    fn union_rings_keeps_disjoint_squares_apart() {
        let merged = union_rings(&[square(0.0, 0.0, 1.0, 1.0), square(5.0, 5.0, 1.0, 1.0)]);
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn ring_paths_roundtrip_and_translate() {
        let ring = square(0.0, 0.0, 1.0, 1.0);
        let paths = ring_to_paths(&ring);
        let back = paths_to_rings(&paths);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].len(), 4);
        for (p, q) in ring.iter().zip(back[0].iter()) {
            assert!(p.dist(q) < 1e-6);
        }
        let moved = translate_paths(&paths, 2.0, 3.0);
        let (minx, miny, maxx, maxy) = ring_bbox(&paths_to_rings(&moved)[0]);
        assert_close(minx, 2.0, 1e-6);
        assert_close(miny, 3.0, 1e-6);
        assert_close(maxx, 3.0, 1e-6);
        assert_close(maxy, 4.0, 1e-6);
    }
}
