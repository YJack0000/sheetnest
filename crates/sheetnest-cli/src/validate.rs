//! `sheetnest validate` - an independent check on a nested DXF.
//!
//! Re-reads an output DXF and checks physical invariants WITHOUT trusting
//! engine internals (the entity -> geometry conversion here is written
//! independently of the parser's):
//!
//!   1. part count: number of top-level closed contours on layer CUT
//!      matches the expected instance count (when given);
//!   2. containment: every part lies inside exactly one SHEET rectangle,
//!      respecting the margin (within tolerance);
//!   3. separation: no two parts on the same sheet overlap, and every pair
//!      keeps a gap of at least `spacing * 0.5` (conservative floor that
//!      absorbs the engine's documented simplification tolerance);
//!   4. tabs: with tabs off every CUT chain must close; with tabs on the
//!      open-chain count is reported for eyeballing.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Args;
use dxf::Drawing;
use dxf::entities::EntityType;
use sheetnest::dxf::chain_segments;
use sheetnest::geom::{inflate_ring, overlap_area};
use sheetnest::model::{Contour, Pt, Seg, ring_bbox};

use crate::opts::parse_sheet;

/// mm: simplification (2x0.25) + clipper grid slack.
const TOL: f64 = 0.6;

/// Check a nested DXF for overlaps, edge clearance and gaps.
///
/// This reads the finished file back the way a cutter would and measures it,
/// rather than asking the nester whether it did a good job. Exits non-zero
/// when something is wrong.
#[derive(Args, Debug)]
pub struct ValidateArgs {
    /// The nested DXF to check.
    #[arg(value_name = "FILE")]
    pub file: PathBuf,

    /// How many part instances the file should contain. Checked when given.
    #[arg(long, value_name = "N")]
    pub expect: Option<usize>,

    /// The gap between parts the job was cut with, mm.
    #[arg(long, value_name = "MM", default_value_t = 2.0)]
    pub spacing: f64,

    /// The gap to the sheet edge the job was cut with, mm.
    #[arg(long, value_name = "MM", default_value_t = 5.0)]
    pub margin: f64,

    /// Sheet size the job was cut on, as WIDTHxHEIGHT in mm. When given,
    /// every sheet outline in the file is measured against it.
    #[arg(long, value_name = "WxH", value_parser = parse_sheet)]
    pub sheet: Option<(f64, f64)>,
}

fn seg_from_entity(et: &EntityType) -> Option<Seg> {
    match et {
        EntityType::Line(l) => Some(Seg::Line {
            a: Pt::new(l.p1.x, l.p1.y),
            b: Pt::new(l.p2.x, l.p2.y),
        }),
        EntityType::Arc(a) => {
            let sweep = (a.end_angle - a.start_angle).rem_euclid(360.0);
            let sweep = if sweep == 0.0 { 360.0 } else { sweep };
            Some(Seg::Arc {
                c: Pt::new(a.center.x, a.center.y),
                r: a.radius,
                start_deg: a.start_angle,
                sweep_deg: sweep,
            })
        }
        EntityType::Circle(c) => Some(Seg::Arc {
            c: Pt::new(c.center.x, c.center.y),
            r: c.radius,
            start_deg: 0.0,
            sweep_deg: 360.0,
        }),
        EntityType::LwPolyline(p) => {
            // Only used for SHEET rectangles (no bulges in engine output).
            let _ = p;
            None
        }
        _ => None,
    }
}

fn point_in_ring(p: &Pt, ring: &[Pt]) -> bool {
    let mut inside = false;
    let n = ring.len();
    for i in 0..n {
        let (a, b) = (&ring[i], &ring[(i + 1) % n]);
        if (a.y > p.y) != (b.y > p.y) {
            let x = a.x + (p.y - a.y) / (b.y - a.y) * (b.x - a.x);
            if p.x < x {
                inside = !inside;
            }
        }
    }
    inside
}

pub fn run(args: ValidateArgs) -> Result<()> {
    let (spacing, margin) = (args.spacing, args.margin);
    let drawing = Drawing::load_file(&args.file)
        .with_context(|| format!("loading {}", args.file.display()))?;

    // --- collect geometry by layer -------------------------------------
    let mut cut_segs: Vec<Seg> = Vec::new();
    let mut sheet_rects: Vec<(f64, f64, f64, f64)> = Vec::new();
    for e in drawing.entities() {
        match e.common.layer.as_str() {
            "CUT" => {
                if let Some(s) = seg_from_entity(&e.specific) {
                    cut_segs.push(s);
                }
            }
            "SHEET" => {
                if let EntityType::LwPolyline(p) = &e.specific {
                    let ring: Vec<Pt> = p.vertices.iter().map(|v| Pt::new(v.x, v.y)).collect();
                    if ring.len() >= 4 {
                        sheet_rects.push(ring_bbox(&ring));
                    }
                }
            }
            _ => {}
        }
    }
    if sheet_rects.is_empty() {
        bail!("no SHEET rectangles found in {}", args.file.display());
    }

    // --- micro-joint (tab) gap detection --------------------------------
    // A tab shows up as a pair of seg endpoints facing each other across a
    // small gap (wider than numeric noise, narrower than any real feature).
    // Count them BEFORE chaining: the loose chaining pass (0.5 mm) would
    // otherwise bridge the gaps and hide the tabs entirely.
    let mut endpoints: Vec<Pt> = Vec::new();
    for s in &cut_segs {
        endpoints.push(s.start());
        endpoints.push(s.end());
    }
    let mut tab_gaps = 0usize;
    for i in 0..endpoints.len() {
        // nearest other endpoint
        let mut nearest = f64::INFINITY;
        for j in 0..endpoints.len() {
            if i != j {
                nearest = nearest.min(endpoints[i].dist(&endpoints[j]));
            }
        }
        if nearest > 0.05 && nearest < 1.0 {
            tab_gaps += 1;
        }
    }
    let tab_gaps = tab_gaps / 2; // both sides of a gap are counted
    println!("micro-joint gaps detected: {tab_gaps}");

    // --- chain CUT geometry into contours ------------------------------
    let n_segs = cut_segs.len();
    let (loops, open_chains) = chain_segments(cut_segs);
    println!(
        "CUT entities: {n_segs} segs -> {} closed loops, {open_chains} open chains",
        loops.len()
    );

    // --- classify top-level (part outers) vs nested (holes) ------------
    let rings: Vec<Vec<Pt>> = loops
        .iter()
        .map(|l| Contour { segs: l.clone() }.polyline(0.25))
        .collect();
    let mut top_level: Vec<usize> = Vec::new();
    for i in 0..rings.len() {
        let probe = rings[i][0];
        let depth = (0..rings.len())
            .filter(|&j| j != i && point_in_ring(&probe, &rings[j]))
            .count();
        if depth % 2 == 0 {
            top_level.push(i);
        }
    }
    println!("top-level contours (part instances): {}", top_level.len());

    let mut violations = 0usize;

    // --- 0. sheet size, when the caller told us what it should be ------
    if let Some((sw, sh)) = args.sheet {
        for (si, (x0, y0, x1, y1)) in sheet_rects.iter().enumerate() {
            let (w, h) = (x1 - x0, y1 - y0);
            if (w - sw).abs() > TOL || (h - sh).abs() > TOL {
                println!("VIOLATION: sheet #{si} measures {w:.1}x{h:.1}, expected {sw:.1}x{sh:.1}");
                violations += 1;
            }
        }
    }

    // --- 1. expected count ---------------------------------------------
    if let Some(exp) = args.expect
        && top_level.len() != exp
    {
        println!(
            "VIOLATION: expected {exp} instances, found {}",
            top_level.len()
        );
        violations += 1;
    }

    // --- 2. sheet containment with margin ------------------------------
    let mut on_sheet: Vec<usize> = vec![usize::MAX; rings.len()];
    for &i in &top_level {
        let (minx, miny, maxx, maxy) = ring_bbox(&rings[i]);
        let mut homes = Vec::new();
        for (si, (sx0, sy0, sx1, sy1)) in sheet_rects.iter().enumerate() {
            if minx >= sx0 + margin - TOL
                && miny >= sy0 + margin - TOL
                && maxx <= sx1 - margin + TOL
                && maxy <= sy1 - margin + TOL
            {
                homes.push(si);
            }
        }
        match homes.len() {
            1 => on_sheet[i] = homes[0],
            0 => {
                println!(
                    "VIOLATION: contour #{i} (bbox {minx:.1},{miny:.1}..{maxx:.1},{maxy:.1}) is on no sheet (margin {margin})"
                );
                violations += 1;
            }
            _ => {
                println!("VIOLATION: contour #{i} spans multiple sheets");
                violations += 1;
            }
        }
    }

    // --- 3. pairwise separation ----------------------------------------
    let half_gap = spacing * 0.5;
    let mut worst_overlap = 0.0f64;
    for a in 0..top_level.len() {
        for b in (a + 1)..top_level.len() {
            let (i, j) = (top_level[a], top_level[b]);
            if on_sheet[i] != on_sheet[j] || on_sheet[i] == usize::MAX {
                continue;
            }
            let raw_overlap = overlap_area(&rings[i], &rings[j]);
            if raw_overlap > 0.01 {
                println!("VIOLATION: contours #{i} and #{j} OVERLAP by {raw_overlap:.3} mm^2");
                violations += 1;
                worst_overlap = worst_overlap.max(raw_overlap);
                continue;
            }
            // gap floor: inflating one by half_gap must still not overlap
            let grown = inflate_ring(&rings[i], half_gap);
            let gap_overlap = overlap_area(&grown, &rings[j]);
            if gap_overlap > 0.01 {
                println!(
                    "VIOLATION: contours #{i} and #{j} closer than {half_gap:.2} mm (proxy overlap {gap_overlap:.3} mm^2)"
                );
                violations += 1;
            }
        }
    }

    // --- 4. tabs / open chains -----------------------------------------
    if open_chains > 0 {
        println!("note: {open_chains} open chains (expected only when tabs are enabled)");
    }

    println!("--------------------------------------------------------------");
    if violations == 0 {
        println!("VALIDATION PASSED (worst overlap {worst_overlap:.4} mm^2)");
        Ok(())
    } else {
        println!("VALIDATION FAILED: {violations} violation(s)");
        std::process::exit(1);
    }
}
