//! Micro-joint ("tab" / 留橋) insertion for laser cutting.
//!
//! A tab is a small gap left UNCUT in a contour so the cut part stays
//! attached to the skeleton and cannot tip up into the laser head. We
//! implement them by splitting each closed contour into open seg chains,
//! removing `width` mm of path at each tab position.
//!
//! Placement rules (laser best practice):
//! - Tab count for a contour = max(min_per_contour for outers / 1 for
//!   holes, ceil(perimeter / max_spacing)).
//! - Ideal positions are evenly distributed along the perimeter (offset by
//!   half a pitch so a tab does not sit exactly at the seam/start point).
//! - Each ideal position snaps to the nearest eligible location: a point
//!   on a Line seg at least `corner_clearance + width/2` from both seg
//!   ends. If the contour has no Line seg with a big enough window (e.g. a
//!   full circle), arcs are allowed with the same clearance rule (distance
//!   measured along the arc); a full-circle contour (single 360-degree arc
//!   seg) has no corners, so any position is eligible.
//! - Two tabs must not end up closer than 10 * width along the perimeter;
//!   drop the later one if they collide.
//! - Holes get tabs only when the hole bbox min-dimension >=
//!   cfg.min_hole_size (small holes drop through the slats harmlessly).
//! - Outer contours always get tabs when cfg.enabled.

use crate::model::{Contour, Seg, TabConfig, ring_bbox};

/// Numeric slop used for arc-length comparisons (mm).
const EPS: f64 = 1e-9;

/// Anything with |sweep| at or above this is treated as a full circle.
const FULL_CIRCLE_DEG: f64 = 359.9;

/// Chord tolerance used only for the hole bbox eligibility test.
const BBOX_TOL: f64 = 0.05;

/// Hard ceiling on tabs per contour, so a pathological `max_spacing`
/// cannot blow up the loop.
const MAX_TABS_PER_CONTOUR: usize = 10_000;

/// Split one closed contour into open chains, leaving `n` uncut gaps.
/// Returns the open chains in cut order (each chain is a consecutive run
/// of segs; arcs are split analytically, never linearized).
///
/// If the config yields zero tabs for this contour (or tab windows cannot
/// be satisfied at all), returns the contour unchanged as a single closed
/// chain (callers treat a chain whose end meets its start as closed).
pub fn apply_tabs_to_contour(contour: &Contour, cfg: &TabConfig, is_hole: bool) -> Vec<Vec<Seg>> {
    plan_and_cut(contour, cfg, is_hole).0
}

/// Convenience: apply to a whole part's contours (outer + holes) in local
/// coordinates. Returns (chains, tab_count_total).
pub fn apply_tabs(outer: &Contour, holes: &[Contour], cfg: &TabConfig) -> (Vec<Vec<Seg>>, usize) {
    let mut chains: Vec<Vec<Seg>> = Vec::new();
    let mut tabs = 0usize;

    let (c, n) = plan_and_cut(outer, cfg, false);
    chains.extend(c);
    tabs += n;

    for h in holes {
        let (c, n) = plan_and_cut(h, cfg, true);
        chains.extend(c);
        tabs += n;
    }

    (chains, tabs)
}

// ---------------------------------------------------------------------------
// planning
// ---------------------------------------------------------------------------

/// Core routine: decide tab positions and cut. Returns (chains, tab count).
/// A tab count of 0 means the contour came back unchanged (still closed).
fn plan_and_cut(contour: &Contour, cfg: &TabConfig, is_hole: bool) -> (Vec<Vec<Seg>>, usize) {
    if contour.segs.is_empty() {
        return (Vec::new(), 0);
    }
    let unchanged = (vec![contour.segs.clone()], 0usize);

    // Tabs off => never modify geometry.
    if !cfg.enabled {
        return unchanged;
    }

    let p = contour.perimeter();
    let w = cfg.width;
    // Degenerate: no width, no perimeter, or a contour so short that a
    // single gap would consume half of it.
    if w.is_nan() || p.is_nan() || w <= 0.0 || p <= 0.0 || p <= 2.0 * w {
        return unchanged;
    }

    // Holes below the size threshold drop through the slats harmlessly.
    if is_hole {
        let ring = contour.polyline(BBOX_TOL);
        if ring.len() < 3 {
            return unchanged;
        }
        let (minx, miny, maxx, maxy) = ring_bbox(&ring);
        let min_dim = (maxx - minx).min(maxy - miny);
        if min_dim < cfg.min_hole_size {
            return unchanged;
        }
    }

    let n = wanted_tab_count(p, w, cfg, is_hole);
    if n == 0 {
        return unchanged;
    }

    let intervals = eligible_intervals(&contour.segs, w, cfg.corner_clearance, p);
    if intervals.is_empty() {
        return unchanged;
    }

    // Ideal centers, offset by half a pitch so no tab sits on the seam.
    let min_sep = 10.0 * w;
    let mut centers: Vec<f64> = Vec::with_capacity(n);
    for i in 0..n {
        let ideal = ((i as f64) + 0.5) * p / (n as f64);
        let Some(pos) = snap_to_eligible(&intervals, ideal, p) else {
            continue;
        };
        // Min perimeter separation: drop the later collider.
        if centers
            .iter()
            .any(|c| cyclic_dist(*c, pos, p) < min_sep - EPS)
        {
            continue;
        }
        // Never remove so much material that nothing is left to cut.
        if ((centers.len() + 1) as f64) * w >= p - EPS {
            break;
        }
        centers.push(pos);
    }

    if centers.is_empty() {
        return unchanged;
    }
    centers.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let chains = cut_gaps(&contour.segs, p, &centers, w);
    if chains.is_empty() {
        return unchanged;
    }
    let tabs = centers.len();
    (chains, tabs)
}

/// max(floor, ceil(perimeter / max_spacing)), capped so the min-separation
/// rule cannot be violated by construction.
fn wanted_tab_count(p: f64, w: f64, cfg: &TabConfig, is_hole: bool) -> usize {
    let by_spacing = if cfg.max_spacing > 0.0 {
        let raw = (p / cfg.max_spacing).ceil();
        if raw.is_finite() && raw > 0.0 {
            (raw as usize).min(MAX_TABS_PER_CONTOUR)
        } else {
            1
        }
    } else {
        1
    };
    let floor_n = if is_hole {
        1
    } else {
        cfg.min_per_contour as usize
    };
    let n = by_spacing.max(floor_n).clamp(1, MAX_TABS_PER_CONTOUR);
    // 10*w minimum separation caps how many can ever fit.
    let fit = ((p / (10.0 * w)).floor() as usize).max(1);
    n.min(fit)
}

/// Perimeter intervals where a tab CENTER may sit.
///
/// A window needs `width + 2 * corner_clearance` of room measured along
/// the segment. Line segs are preferred; arcs are only used when no Line
/// seg qualifies. A single full-circle arc has no corners at all, so the
/// whole perimeter is eligible.
fn eligible_intervals(segs: &[Seg], w: f64, corner_clearance: f64, p: f64) -> Vec<(f64, f64)> {
    if segs.len() == 1
        && let Seg::Arc { sweep_deg, .. } = &segs[0]
        && sweep_deg.abs() >= FULL_CIRCLE_DEG
    {
        return vec![(0.0, p)];
    }

    let half = w / 2.0 + corner_clearance.max(0.0);
    let mut lines: Vec<(f64, f64)> = Vec::new();
    let mut arcs: Vec<(f64, f64)> = Vec::new();
    let mut off = 0.0;
    for s in segs {
        let l = s.length();
        if l >= 2.0 * half {
            let iv = (off + half, off + l - half);
            match s {
                Seg::Line { .. } => lines.push(iv),
                Seg::Arc { .. } => arcs.push(iv),
            }
        }
        off += l;
    }

    if !lines.is_empty() { lines } else { arcs }
}

/// Nearest eligible perimeter offset to `ideal` (cyclic metric).
fn snap_to_eligible(intervals: &[(f64, f64)], ideal: f64, p: f64) -> Option<f64> {
    let mut best: Option<(f64, f64)> = None; // (distance, position)
    for &(lo, hi) in intervals {
        let cand = if ideal >= lo && ideal <= hi {
            (0.0, ideal)
        } else {
            let dlo = cyclic_dist(ideal, lo, p);
            let dhi = cyclic_dist(ideal, hi, p);
            if dlo <= dhi { (dlo, lo) } else { (dhi, hi) }
        };
        match best {
            Some((bd, _)) if bd <= cand.0 => {}
            _ => best = Some(cand),
        }
    }
    best.map(|(_, pos)| pos)
}

// ---------------------------------------------------------------------------
// cutting
// ---------------------------------------------------------------------------

/// Remove a `w`-wide gap around each center and return the surviving open
/// chains. `centers` must be sorted ascending and lie in [0, p).
///
/// The chain that used to wrap the contour's seam is produced naturally:
/// every chain starts at the END of one gap and runs forward (wrapping
/// past the seam if needed) to the START of the next gap, which is exactly
/// "rotate the seam to the first gap start".
fn cut_gaps(segs: &[Seg], p: f64, centers: &[f64], w: f64) -> Vec<Vec<Seg>> {
    let cum = cumulative(segs, p);
    let m = centers.len();
    let mut chains: Vec<Vec<Seg>> = Vec::with_capacity(m);
    for j in 0..m {
        let gap_end = wrap(centers[j] + w / 2.0, p);
        let next_gap_start = wrap(centers[(j + 1) % m] - w / 2.0, p);
        let mut keep = next_gap_start - gap_end;
        if keep <= 0.0 {
            keep += p;
        }
        if keep <= EPS {
            continue;
        }
        let chain = sub_chain(segs, &cum, p, gap_end, keep);
        if !chain.is_empty() {
            chains.push(chain);
        }
    }
    chains
}

/// cum[i] = perimeter offset where seg i starts; cum[len] = p.
fn cumulative(segs: &[Seg], p: f64) -> Vec<f64> {
    let mut cum = Vec::with_capacity(segs.len() + 1);
    let mut off = 0.0;
    for s in segs {
        cum.push(off);
        off += s.length();
    }
    cum.push(p);
    cum
}

/// The open run of segs starting at perimeter offset `start`, `len` long,
/// wrapping past the seam when necessary.
fn sub_chain(segs: &[Seg], cum: &[f64], p: f64, start: f64, len: f64) -> Vec<Seg> {
    let n = segs.len();
    if n == 0 {
        return Vec::new();
    }
    let pos = wrap(start, p);
    let mut idx = locate(cum, pos);
    let mut local = (pos - cum[idx]).max(0.0);
    let mut remaining = len.min(p);
    let mut out: Vec<Seg> = Vec::new();

    // Each iteration either emits a piece or advances a seg; the guard is
    // only there so a ring of zero-length segs cannot spin forever.
    let mut guard = 0usize;
    let limit = 4 * n + 8;
    while remaining > EPS && guard < limit {
        guard += 1;
        let sl = segs[idx].length();
        let avail = sl - local;
        if avail <= EPS {
            idx = (idx + 1) % n;
            local = 0.0;
            continue;
        }
        let take = remaining.min(avail);
        if let Some(piece) = seg_slice(&segs[idx], local, local + take) {
            out.push(piece);
        }
        remaining -= take;
        local += take;
        if local >= sl - EPS {
            idx = (idx + 1) % n;
            local = 0.0;
        }
    }
    out
}

/// Largest i with cum[i] <= pos, clamped to a valid seg index.
fn locate(cum: &[f64], pos: f64) -> usize {
    let n = cum.len() - 1; // number of segs
    let mut idx = 0usize;
    for (i, &c) in cum.iter().enumerate().take(n) {
        if c <= pos + EPS {
            idx = i;
        } else {
            break;
        }
    }
    idx.min(n.saturating_sub(1))
}

/// The piece of `s` between arc-length `from` and `to`, or None if empty.
fn seg_slice(s: &Seg, from: f64, to: f64) -> Option<Seg> {
    let len = s.length();
    let a = from.clamp(0.0, len);
    let b = to.clamp(0.0, len);
    if b - a <= EPS {
        return None;
    }
    let tail = if a > EPS { s.split_at(a).1 } else { s.clone() };
    let want = b - a;
    if want < tail.length() - EPS {
        Some(tail.split_at(want).0)
    } else {
        Some(tail)
    }
}

fn wrap(v: f64, p: f64) -> f64 {
    if p <= 0.0 {
        return 0.0;
    }
    let mut r = v % p;
    if r < 0.0 {
        r += p;
    }
    r
}

fn cyclic_dist(a: f64, b: f64, p: f64) -> f64 {
    let d = wrap(a - b, p);
    d.min(p - d)
}

#[cfg(test)]
mod tests {
    // Required tests (implementer: keep these, add more):
    // - 100x100 square, width=0.5, min_per_contour=2, max_spacing=1000:
    //   exactly 2 gaps, total open-chain length ~ 400 - 2*0.5, every gap on
    //   a Line seg, >= corner_clearance from corners.
    // - perimeter 800 square with max_spacing 200 -> 4 tabs.
    // - full circle r=10 gets tabs even though it has no Line segs.
    // - hole with bbox 20x20 and min_hole_size 40 -> unchanged closed chain.
    // - chains reassemble: sum of chain lengths + gaps == original
    //   perimeter (1e-6).
    use super::*;
    use crate::model::Pt;

    fn square(x0: f64, y0: f64, side: f64) -> Contour {
        let p = [
            Pt::new(x0, y0),
            Pt::new(x0 + side, y0),
            Pt::new(x0 + side, y0 + side),
            Pt::new(x0, y0 + side),
        ];
        Contour {
            segs: (0..4)
                .map(|i| Seg::Line {
                    a: p[i],
                    b: p[(i + 1) % 4],
                })
                .collect(),
        }
    }

    fn circle(cx: f64, cy: f64, r: f64) -> Contour {
        Contour {
            segs: vec![Seg::Arc {
                c: Pt::new(cx, cy),
                r,
                start_deg: 0.0,
                sweep_deg: 360.0,
            }],
        }
    }

    fn cfg(width: f64, max_spacing: f64, min_per_contour: u32) -> TabConfig {
        TabConfig {
            enabled: true,
            width,
            max_spacing,
            min_per_contour,
            corner_clearance: 3.0,
            min_hole_size: 40.0,
        }
    }

    fn chain_len(chain: &[Seg]) -> f64 {
        chain.iter().map(|s| s.length()).sum()
    }

    fn total_len(chains: &[Vec<Seg>]) -> f64 {
        chains.iter().map(|c| chain_len(c)).sum()
    }

    fn is_closed(chain: &[Seg]) -> bool {
        !chain.is_empty() && chain[0].start().dist(&chain[chain.len() - 1].end()) < 1e-6
    }

    /// Every consecutive pair of segs in a chain must actually connect.
    fn assert_continuous(chain: &[Seg]) {
        for w in chain.windows(2) {
            assert!(
                w[0].end().dist(&w[1].start()) < 1e-9,
                "chain break: {:?} -> {:?}",
                w[0].end(),
                w[1].start()
            );
        }
    }

    #[test]
    fn square_gets_two_tabs_on_line_segs() {
        let c = square(0.0, 0.0, 100.0);
        let cfg = cfg(0.5, 1000.0, 2);
        let (chains, tabs) = apply_tabs(&c, &[], &cfg);

        assert_eq!(tabs, 2, "expected exactly 2 tabs");
        assert_eq!(chains.len(), 2, "2 gaps -> 2 open chains");

        // Total cut length = perimeter - tabs * width.
        let expect = 400.0 - 2.0 * 0.5;
        assert!(
            (total_len(&chains) - expect).abs() < 1e-9,
            "total chain length {} != {}",
            total_len(&chains),
            expect
        );

        // Chains are open and internally continuous.
        for ch in &chains {
            assert!(!is_closed(ch), "chain should be open");
            assert_continuous(ch);
        }

        // Every gap boundary (= chain endpoint) sits on a straight side,
        // at least corner_clearance away from any corner.
        let corners = [
            Pt::new(0.0, 0.0),
            Pt::new(100.0, 0.0),
            Pt::new(100.0, 100.0),
            Pt::new(0.0, 100.0),
        ];
        for ch in &chains {
            for pt in [ch[0].start(), ch[ch.len() - 1].end()] {
                // on the boundary of the square (one coord at 0 or 100)
                let on_edge = (pt.x.abs() < 1e-9 || (pt.x - 100.0).abs() < 1e-9)
                    || (pt.y.abs() < 1e-9 || (pt.y - 100.0).abs() < 1e-9);
                assert!(on_edge, "gap endpoint {pt:?} not on a side");
                for c in &corners {
                    assert!(
                        pt.dist(c) >= 3.0 - 1e-9,
                        "gap endpoint {pt:?} too close to corner {c:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn spacing_drives_tab_count() {
        // 200x200 square -> perimeter 800; max_spacing 200 -> 4 tabs.
        let c = square(0.0, 0.0, 200.0);
        assert!((c.perimeter() - 800.0).abs() < 1e-9);
        let (chains, tabs) = apply_tabs(&c, &[], &cfg(0.5, 200.0, 2));
        assert_eq!(tabs, 4);
        assert_eq!(chains.len(), 4);
        assert!((total_len(&chains) - (800.0 - 4.0 * 0.5)).abs() < 1e-9);
    }

    #[test]
    fn full_circle_gets_tabs_without_line_segs() {
        let c = circle(0.0, 0.0, 10.0);
        let p = c.perimeter();
        let (chains, tabs) = apply_tabs(&c, &[], &cfg(0.5, 1000.0, 2));
        assert_eq!(tabs, 2, "full circle should still get tabs");
        assert_eq!(chains.len(), 2);
        for ch in &chains {
            assert!(!is_closed(ch));
            assert_continuous(ch);
            for s in ch {
                assert!(matches!(s, Seg::Arc { .. }), "circle pieces stay arcs");
            }
        }
        assert!((total_len(&chains) - (p - 2.0 * 0.5)).abs() < 1e-9);
    }

    #[test]
    fn small_hole_is_left_closed() {
        let h = square(0.0, 0.0, 20.0); // bbox 20x20 < min_hole_size 40
        let chains = apply_tabs_to_contour(&h, &cfg(0.5, 1000.0, 2), true);
        assert_eq!(chains.len(), 1);
        assert!(is_closed(&chains[0]), "small hole must stay closed");
        assert_eq!(chains[0], h.segs);
    }

    #[test]
    fn large_hole_gets_at_least_one_tab() {
        let h = square(0.0, 0.0, 60.0); // bbox 60x60 >= 40
        let cfg = cfg(0.5, 1000.0, 2);
        let chains = apply_tabs_to_contour(&h, &cfg, true);
        // holes floor at 1, ceil(240/1000) = 1
        assert_eq!(chains.len(), 1);
        assert!(!is_closed(&chains[0]), "large hole should be cut open");
        assert!((chain_len(&chains[0]) - (240.0 - 0.5)).abs() < 1e-9);
        assert_continuous(&chains[0]);
    }

    #[test]
    fn chains_reassemble_to_perimeter() {
        for (side, w, spacing, minc) in [
            (100.0, 0.5, 1000.0, 2u32),
            (200.0, 0.3, 200.0, 2),
            (37.5, 0.2, 25.0, 3),
            (60.0, 1.0, 1000.0, 1),
        ] {
            let c = square(0.0, 0.0, side);
            let p = c.perimeter();
            let (chains, tabs) = apply_tabs(&c, &[], &cfg(w, spacing, minc));
            let got = total_len(&chains) + tabs as f64 * w;
            assert!(
                (got - p).abs() < 1e-6,
                "side {side}: reassembled {got} != perimeter {p} (tabs {tabs})"
            );
        }
    }

    #[test]
    fn part_level_chains_cover_outer_and_holes() {
        let outer = square(0.0, 0.0, 300.0);
        let big_hole = square(20.0, 20.0, 60.0);
        let small_hole = square(200.0, 200.0, 10.0);
        let cfg = cfg(0.5, 1000.0, 2);
        let (chains, tabs) = apply_tabs(&outer, &[big_hole.clone(), small_hole.clone()], &cfg);

        // outer: 2 tabs, big hole: 1 tab, small hole: none (closed chain).
        assert_eq!(tabs, 3);
        assert_eq!(chains.len(), 2 + 1 + 1);
        let closed: Vec<_> = chains.iter().filter(|c| is_closed(c)).collect();
        assert_eq!(closed.len(), 1, "only the small hole stays closed");
        assert_eq!(*closed[0], small_hole.segs);

        let expect = outer.perimeter() + big_hole.perimeter() + small_hole.perimeter() - 3.0 * 0.5;
        assert!((total_len(&chains) - expect).abs() < 1e-6);
    }

    #[test]
    fn disabled_config_leaves_geometry_untouched() {
        let c = square(0.0, 0.0, 100.0);
        let mut cfg = cfg(0.5, 1000.0, 2);
        cfg.enabled = false;
        let (chains, tabs) = apply_tabs(&c, &[], &cfg);
        assert_eq!(tabs, 0);
        assert_eq!(chains, vec![c.segs.clone()]);
    }

    #[test]
    fn tiny_contour_is_left_closed() {
        // perimeter 0.4mm with width 0.5mm -> nothing sensible to cut.
        let c = square(0.0, 0.0, 0.1);
        let chains = apply_tabs_to_contour(&c, &cfg(0.5, 1000.0, 2), false);
        assert_eq!(chains.len(), 1);
        assert!(is_closed(&chains[0]));
    }

    #[test]
    fn no_eligible_window_leaves_contour_closed() {
        // 5mm sides, corner_clearance 3mm -> needs 6.5mm per side.
        let c = square(0.0, 0.0, 5.0);
        let chains = apply_tabs_to_contour(&c, &cfg(0.5, 1000.0, 2), false);
        assert_eq!(chains.len(), 1);
        assert!(is_closed(&chains[0]));
    }

    #[test]
    fn min_separation_drops_colliding_tabs() {
        // Perimeter 40; width 2 -> min separation 20 -> at most 2 tabs even
        // though max_spacing asks for 8.
        let c = square(0.0, 0.0, 10.0);
        let mut cfg = cfg(2.0, 5.0, 2);
        cfg.corner_clearance = 0.5;
        let (chains, tabs) = apply_tabs(&c, &[], &cfg);
        assert!(tabs <= 2, "got {tabs} tabs, expected <= 2");
        assert_eq!(chains.len(), tabs);
        assert!((total_len(&chains) + tabs as f64 * 2.0 - 40.0).abs() < 1e-6);
    }

    #[test]
    fn single_tab_chain_wraps_the_seam() {
        // One tab placed on the FIRST side means the surviving chain has to
        // wrap past the contour's start point.
        let c = square(0.0, 0.0, 100.0);
        let cfg = cfg(0.5, 1000.0, 1);
        let chains = apply_tabs_to_contour(&c, &cfg, false);
        assert_eq!(chains.len(), 1);
        let ch = &chains[0];
        assert!(!is_closed(ch));
        assert_continuous(ch);
        assert!((chain_len(ch) - (400.0 - 0.5)).abs() < 1e-9);
        // ideal center = 200 (a corner) -> snaps back to 196.75 on the
        // second side, so the chain starts at offset 197 = (100, 97) and
        // has to wrap past the seam at (0,0) to reach offset 196.5.
        assert!(
            (ch[0].start().x - 100.0).abs() < 1e-9,
            "{:?}",
            ch[0].start()
        );
        assert!((ch[0].start().y - 97.0).abs() < 1e-9, "{:?}", ch[0].start());
        let last = ch[ch.len() - 1].end();
        assert!((last.x - 100.0).abs() < 1e-9, "{last:?}");
        assert!((last.y - 96.5).abs() < 1e-9, "{last:?}");
        // 4 original sides, and the wrap splits the second side in two.
        assert_eq!(ch.len(), 5);
    }

    #[test]
    fn arcs_are_split_analytically() {
        // Rounded slot: two lines + two half-circle arcs.
        let segs = vec![
            Seg::Line {
                a: Pt::new(0.0, 0.0),
                b: Pt::new(100.0, 0.0),
            },
            Seg::Arc {
                c: Pt::new(100.0, 10.0),
                r: 10.0,
                start_deg: -90.0,
                sweep_deg: 180.0,
            },
            Seg::Line {
                a: Pt::new(100.0, 20.0),
                b: Pt::new(0.0, 20.0),
            },
            Seg::Arc {
                c: Pt::new(0.0, 10.0),
                r: 10.0,
                start_deg: 90.0,
                sweep_deg: 180.0,
            },
        ];
        let c = Contour { segs };
        let p = c.perimeter();
        let (chains, tabs) = apply_tabs(&c, &[], &cfg(0.4, 60.0, 2));
        assert!(tabs >= 2);
        for ch in &chains {
            assert_continuous(ch);
        }
        assert!((total_len(&chains) + tabs as f64 * 0.4 - p).abs() < 1e-6);
        // Arcs survive as arcs (nothing linearized).
        let arc_count: usize = chains
            .iter()
            .flat_map(|c| c.iter())
            .filter(|s| matches!(s, Seg::Arc { .. }))
            .count();
        assert!(arc_count >= 2, "arcs must stay analytic");
    }
}
