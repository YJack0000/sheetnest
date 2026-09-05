//! Genetic algorithm over placement order + rotations.
//!
//! Individual = `Vec<Gene>` (a permutation of all part instances, each with a
//! rotation index). Fitness = place::decode(...).fitness (lower better).
//!
//! - Instances: parts expanded by quantity. `instance_part[i]` gives the
//!   part index for instance i.
//! - Initial population: individual 0 sorts instances by gross_area
//!   descending with rotation 0 (greedy heuristic); the rest are random
//!   shuffles with random rotations.
//! - Selection: tournament of 2. Elitism: best 2 carried over unchanged.
//! - Crossover: order crossover (OX) on the instance permutation; child
//!   genes keep the rotation of whichever parent contributed the instance.
//! - Mutation: per gene with probability cfg.mutation_rate: swap with the
//!   next gene; independently, with the same probability, re-roll the
//!   rotation index.
//! - Population fitness evaluation runs in parallel with rayon when the
//!   `parallel` feature is on; the NfpCache is shared across threads.
//!   Decode is deterministic per gene sequence, so the feature changes
//!   speed only, never the result.
//! - Randomness comes from a `StdRng` seeded by `cfg.seed` (or a fresh
//!   random seed), so a seeded run that ends on `stale_generations` is
//!   reproducible.
//! - Termination: cfg.time_limit_ms elapsed, cfg.stale_generations
//!   consecutive generations without fitness improvement, or the caller's
//!   `should_stop` hook. Always returns the best individual seen.
//!
//! Progress: `Hooks::on_progress` is called once per generation from the
//! calling thread. The utilization reported is measured against the stock
//! actually consumed under `auto_width`, and against the configured sheets
//! otherwise — the sheet-area ratio is meaningless when the width is only a
//! search bound.

use crate::geom;
use crate::model::{
    NestConfig, NestSolution, NestStats, Part, Placement, Progress, StopReason, ring_bbox,
};
use crate::place::{self, DecodeResult, Gene, NfpCache, PreppedPart};
use anyhow::bail;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::{RngExt, SeedableRng};
#[cfg(feature = "parallel")]
use rayon::prelude::*;
use web_time::Instant;

/// Optional callbacks into a running nest.
///
/// Both are invoked once per generation, on the thread that called
/// [`run_nest`]. `on_progress` comes first; if `should_stop` then returns
/// true the run ends with [`StopReason::Cancelled`] and the best layout so
/// far.
/// Per-generation progress callback.
pub type ProgressFn = Box<dyn FnMut(&Progress) + Send>;
/// Cancellation check, polled once per generation.
pub type StopFn = Box<dyn Fn() -> bool + Send + Sync>;

#[derive(Default)]
pub struct Hooks {
    pub on_progress: Option<ProgressFn>,
    pub should_stop: Option<StopFn>,
}

impl Hooks {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn on_progress(mut self, f: impl FnMut(&Progress) + Send + 'static) -> Self {
        self.on_progress = Some(Box::new(f));
        self
    }

    pub fn should_stop(mut self, f: impl Fn() -> bool + Send + Sync + 'static) -> Self {
        self.should_stop = Some(Box::new(f));
        self
    }
}

/// Minimum fitness decrease that counts as an improvement.
const IMPROVE_EPS: f64 = 1e-9;
/// Overlap area threshold for the final sanity check, mm^2.
const VERIFY_EPS_AREA: f64 = 0.01;

/// Tournament-of-2 selection: lower fitness wins.
fn tournament<'a>(pop: &'a [Vec<Gene>], fits: &[f64], rng: &mut StdRng) -> &'a [Gene] {
    let a = rng.random_range(0..pop.len());
    let b = rng.random_range(0..pop.len());
    if fits[a] <= fits[b] { &pop[a] } else { &pop[b] }
}

/// Order crossover (OX): child takes p1's genes (with p1's rotations) in a
/// random window, then fills the remaining slots cyclically from p2 in p2's
/// order, keeping p2's rotation genes. `n_instances` is the dense instance
/// id space (0..n).
fn ox_crossover(p1: &[Gene], p2: &[Gene], n_instances: usize, rng: &mut StdRng) -> Vec<Gene> {
    let n = p1.len();
    if n == 0 {
        return Vec::new();
    }
    let (mut i, mut j) = (rng.random_range(0..n), rng.random_range(0..n));
    if i > j {
        std::mem::swap(&mut i, &mut j);
    }
    let mut child: Vec<Option<Gene>> = vec![None; n];
    let mut used = vec![false; n_instances];
    for k in i..=j {
        child[k] = Some(p1[k]);
        used[p1[k].instance] = true;
    }
    let mut pos = (j + 1) % n;
    for off in 0..n {
        let g = p2[(j + 1 + off) % n];
        if used[g.instance] {
            continue;
        }
        child[pos] = Some(g);
        used[g.instance] = true;
        pos = (pos + 1) % n;
    }
    child.into_iter().map(|g| g.unwrap()).collect()
}

/// Per-gene mutation: swap with the next gene with probability `rate`;
/// independently re-roll the rotation index with the same probability.
fn mutate(genes: &mut [Gene], rate: f64, n_rot: usize, rng: &mut StdRng) {
    let n = genes.len();
    for k in 0..n {
        if k + 1 < n && rng.random_bool(rate) {
            genes.swap(k, k + 1);
        }
        if rng.random_bool(rate) {
            genes[k].rotation = rng.random_range(0..n_rot);
        }
    }
}

/// Decode every individual. Parallel with rayon when the feature is on;
/// the output order matches the population order either way.
fn evaluate(
    population: &[Vec<Gene>],
    prepped: &[PreppedPart],
    instance_part: &[usize],
    cfg: &NestConfig,
    cache: &NfpCache,
) -> Vec<DecodeResult> {
    #[cfg(feature = "parallel")]
    {
        population
            .par_iter()
            .map(|g| place::decode(g, prepped, instance_part, cfg, cache))
            .collect()
    }
    #[cfg(not(feature = "parallel"))]
    {
        population
            .iter()
            .map(|g| place::decode(g, prepped, instance_part, cfg, cache))
            .collect()
    }
}

/// utilization = net area of placed instances / (sheets_used * sheet area).
///
/// Only meaningful when the sheet width is a real material limit. Under
/// `auto_width` the width is a search bound far wider than anything used, so
/// this ratio collapses towards zero and says nothing — use
/// [`strip_utilization_of`] there instead.
fn utilization(
    res: &DecodeResult,
    prepped: &[PreppedPart],
    instance_part: &[usize],
    sheet_area: f64,
) -> f64 {
    let sheets_used = res.placed.iter().map(|p| p.sheet + 1).max().unwrap_or(0);
    if sheets_used == 0 || sheet_area <= 0.0 {
        return 0.0;
    }
    let net: f64 = net_area(res, prepped, instance_part);
    net / (sheets_used as f64 * sheet_area)
}

fn net_area(res: &DecodeResult, prepped: &[PreppedPart], instance_part: &[usize]) -> f64 {
    res.placed
        .iter()
        .map(|p| prepped[instance_part[p.instance]].net_area)
        .sum()
}

/// Utilization measured against the stock actually consumed — net placed area
/// over `used_width * sheet_height` — so it stays meaningful whether the width
/// was pinned by the operator or found by the nester.
///
/// `decode`'s fitness *is* that strip area: it is defined as
/// `(sheets_used - 1) * sheet_area + last_max_x * sheet_height`, which factors
/// to `used_width * sheet_height`. That identity only holds when nothing was
/// left unplaced, since unplaceable instances add a penalty term.
fn strip_utilization_of(
    res: &DecodeResult,
    prepped: &[PreppedPart],
    instance_part: &[usize],
) -> f64 {
    if !res.unplaceable.is_empty() || res.fitness <= 0.0 {
        return 0.0;
    }
    net_area(res, prepped, instance_part) / res.fitness
}

/// A sheet width wide enough that every instance fits in a single row, so the
/// nester never has to open a second sheet and is free to minimise the length
/// it actually consumes.
///
/// Each instance contributes the larger of its two bbox sides (any allowed
/// rotation is then covered) plus the inter-part spacing; the sheet margins are
/// added once. Deliberately a loose bound: it only caps the search space, and
/// `stats.used_width` reports what was really used.
fn single_row_width_bound(
    prepped: &[PreppedPart],
    instance_part: &[usize],
    cfg: &NestConfig,
) -> f64 {
    let mut width = 2.0 * cfg.margin;
    for &pi in instance_part {
        let (minx, miny, maxx, maxy) = ring_bbox(&prepped[pi].raw);
        width += (maxx - minx).max(maxy - miny) + cfg.spacing;
    }
    // A degenerate part set must still yield a usable sheet.
    width.max(1.0)
}

/// Run the full nest: prep parts, expand instances, evolve, decode best,
/// verify no overlaps (place::verify_no_overlaps; on violation, return an
/// error — this is a bug guard, not an expected path), and assemble the
/// final `NestSolution` (placements sorted by sheet then part id).
pub fn run_nest(parts: &[Part], cfg: &NestConfig, hooks: Hooks) -> anyhow::Result<NestSolution> {
    let start = Instant::now();
    let mut warnings: Vec<String> = Vec::new();
    let mut hooks = hooks;
    let prepped = place::prep_parts(parts, cfg);

    // Expand parts into instances by quantity.
    let mut instance_part: Vec<usize> = Vec::new();
    let mut instance_no: Vec<u32> = Vec::new();
    for (pi, p) in parts.iter().enumerate() {
        for k in 0..p.quantity {
            instance_part.push(pi);
            instance_no.push(k);
        }
    }
    let total = instance_part.len();

    // In auto-width mode the configured sheet width is replaced by a bound
    // wide enough for a single row. Everything downstream (placement, fitness,
    // SVG) then works with one unbounded strip, which is what makes the
    // width-minimising fitness see the whole layout: with a bounded sheet that
    // overflows, the filled sheets contribute a constant and only the tail on
    // the last sheet is still under optimisation.
    let user_cfg = cfg;
    let auto_cfg;
    let cfg = if cfg.auto_width && total > 0 {
        auto_cfg = NestConfig {
            sheet_width: single_row_width_bound(&prepped, &instance_part, cfg),
            ..cfg.clone()
        };
        &auto_cfg
    } else {
        cfg
    };

    let sheet_area = cfg.sheet_width * cfg.sheet_height;

    if total == 0 {
        return Ok(NestSolution {
            placements: Vec::new(),
            stats: NestStats {
                stop_reason: StopReason::Empty,
                sheets_used: 0,
                used_width: 0.0,
                utilization: 0.0,
                strip_utilization: 0.0,
                generations: 0,
                elapsed_ms: start.elapsed().as_millis() as u64,
                placed: 0,
                total: 0,
            },
            sheet_width: user_cfg.sheet_width,
            sheet_height: user_cfg.sheet_height,
            warnings,
        });
    }

    let rotations = cfg.allowed_rotations();
    let n_rot = rotations.len();
    let cache = NfpCache::new();
    let seed = cfg.seed.unwrap_or_else(rand::random::<u64>);
    let mut rng = StdRng::seed_from_u64(seed);

    // ---- initial population ------------------------------------------
    let pop_size = cfg.population.max(3);
    let mut order: Vec<usize> = (0..total).collect();
    order.sort_by(|&a, &b| {
        let aa = prepped[instance_part[a]].gross_area;
        let bb = prepped[instance_part[b]].gross_area;
        bb.partial_cmp(&aa)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.cmp(&b))
    });
    let greedy: Vec<Gene> = order
        .into_iter()
        .map(|i| Gene {
            instance: i,
            rotation: 0,
        })
        .collect();
    let mut population: Vec<Vec<Gene>> = Vec::with_capacity(pop_size);
    population.push(greedy);
    while population.len() < pop_size {
        let mut idx: Vec<usize> = (0..total).collect();
        idx.shuffle(&mut rng);
        population.push(
            idx.into_iter()
                .map(|i| Gene {
                    instance: i,
                    rotation: rng.random_range(0..n_rot),
                })
                .collect(),
        );
    }

    // ---- evolution loop ----------------------------------------------
    let mutation_rate = cfg.mutation_rate.clamp(0.0, 1.0);
    let mut best_fit = f64::INFINITY;
    let mut best_result: Option<DecodeResult> = None;
    let mut best_util = 0.0;
    let mut generations: u32 = 0;
    let mut stale: u32 = 0;
    let stop_reason;

    loop {
        let mut results = evaluate(&population, &prepped, &instance_part, cfg, &cache);
        generations += 1;
        let fits: Vec<f64> = results.iter().map(|r| r.fitness).collect();

        let gi = (0..fits.len())
            .min_by(|&a, &b| {
                fits[a]
                    .partial_cmp(&fits[b])
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .unwrap();
        if fits[gi] < best_fit - IMPROVE_EPS {
            best_fit = fits[gi];
            let gen_best = results.swap_remove(gi);
            // Progress reports the strip-based ratio: under auto_width the
            // sheet-area ratio is measured against the search bound and would
            // crawl near zero for a perfectly good layout.
            best_util = if cfg.auto_width {
                strip_utilization_of(&gen_best, &prepped, &instance_part)
            } else {
                utilization(&gen_best, &prepped, &instance_part, sheet_area)
            };
            best_result = Some(gen_best);
            stale = 0;
        } else {
            stale += 1;
        }

        let elapsed = start.elapsed().as_millis() as u64;
        if let Some(cb) = hooks.on_progress.as_mut() {
            cb(&Progress {
                generation: generations,
                best_fitness: best_fit,
                best_utilization: best_util,
                elapsed_ms: elapsed,
            });
        }

        if hooks.should_stop.as_ref().is_some_and(|f| f()) {
            stop_reason = StopReason::Cancelled;
            break;
        }
        if elapsed >= cfg.time_limit_ms {
            stop_reason = StopReason::TimeLimit;
            break;
        }
        if stale >= cfg.stale_generations.max(1) {
            stop_reason = StopReason::Stale;
            break;
        }

        // ---- breed the next generation --------------------------------
        let mut idxs: Vec<usize> = (0..population.len()).collect();
        idxs.sort_by(|&a, &b| {
            fits[a]
                .partial_cmp(&fits[b])
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut next: Vec<Vec<Gene>> = Vec::with_capacity(pop_size);
        // Elitism: best 2 carried over unchanged.
        next.push(population[idxs[0]].clone());
        next.push(population[idxs[1]].clone());
        while next.len() < pop_size {
            let p1 = tournament(&population, &fits, &mut rng);
            let p2 = tournament(&population, &fits, &mut rng);
            let mut child = ox_crossover(p1, p2, total, &mut rng);
            mutate(&mut child, mutation_rate, n_rot, &mut rng);
            next.push(child);
        }
        population = next;
    }

    // ---- assemble the solution from the best decode -------------------
    // best_result is the decode of the best individual (decode is
    // deterministic, so no re-decode is needed).
    let best = best_result.expect("at least one generation was evaluated");

    let violations =
        place::verify_no_overlaps(&best.placed, &prepped, &instance_part, VERIFY_EPS_AREA);
    if !violations.is_empty() {
        bail!(
            "nesting engine bug: {} overlapping placement pair(s): {:?}",
            violations.len(),
            violations
        );
    }

    if !best.unplaceable.is_empty() {
        warnings.push(format!(
            "{} instance(s) could not be placed on any sheet (part larger than the usable sheet area)",
            best.unplaceable.len()
        ));
    }

    let mut placements: Vec<Placement> = best
        .placed
        .iter()
        .map(|p| {
            let pi = instance_part[p.instance];
            Placement {
                part_id: pi,
                part_name: parts[pi].name.clone(),
                instance: instance_no[p.instance],
                sheet: p.sheet,
                rotation_deg: p.rotation_deg,
                dx: p.dx,
                dy: p.dy,
            }
        })
        .collect();
    placements.sort_by_key(|p| (p.sheet, p.part_id, p.instance));

    let sheets_used = best.placed.iter().map(|p| p.sheet + 1).max().unwrap_or(0);
    let net_placed: f64 = best
        .placed
        .iter()
        .map(|p| prepped[instance_part[p.instance]].net_area)
        .sum();
    // Under auto_width there is no operator-chosen sheet to measure against —
    // the strip the nester settled on is the sheet — so both ratios describe
    // the same thing and `utilization` stays a number worth showing.
    let sheet_ratio = if sheets_used > 0 && sheet_area > 0.0 {
        net_placed / (sheets_used as f64 * sheet_area)
    } else {
        0.0
    };
    // Strip utilization: full area of all sheets but the last, plus the
    // used bounding strip (max world x * sheet height) of the last sheet.
    let mut last_max_x: f64 = 0.0;
    if sheets_used > 0 {
        for p in &best.placed {
            if p.sheet + 1 != sheets_used {
                continue;
            }
            let raw = &prepped[instance_part[p.instance]].raw;
            let (_, _, maxx, _) = ring_bbox(&geom::rotate_ring(raw, p.rotation_deg));
            last_max_x = last_max_x.max(p.dx + maxx);
        }
    }
    // Stock consumed along X: the full sheets before the last, plus how far
    // into the last sheet the layout reaches. Same quantity the fitness
    // function minimises, just without the constant sheet_height factor.
    let used_width = sheets_used.saturating_sub(1) as f64 * cfg.sheet_width + last_max_x;
    let strip_area = used_width * cfg.sheet_height;
    let strip_utilization = if strip_area > 0.0 {
        net_placed / strip_area
    } else {
        0.0
    };
    let utilization = if cfg.auto_width {
        strip_utilization
    } else {
        sheet_ratio
    };

    // Under auto_width the nest ran against a single-row bound far wider
    // than anything used; the sheet to draw and cut is the length reached
    // plus one margin so parts are not flush against the cut edge.
    let sheet_width = if user_cfg.auto_width {
        used_width + user_cfg.margin
    } else {
        user_cfg.sheet_width
    };

    Ok(NestSolution {
        placements,
        stats: NestStats {
            stop_reason,
            sheets_used,
            used_width,
            utilization,
            strip_utilization,
            generations,
            elapsed_ms: start.elapsed().as_millis() as u64,
            placed: best.placed.len(),
            total,
        },
        sheet_width,
        sheet_height: user_cfg.sheet_height,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Pt, RotationMode};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn rect_part(name: &str, w: f64, h: f64, qty: u32) -> Part {
        let pts = [
            Pt::new(0.0, 0.0),
            Pt::new(w, 0.0),
            Pt::new(w, h),
            Pt::new(0.0, h),
        ];
        Part::from_polygon(name, qty, &pts, &[]).unwrap()
    }

    /// Auto width must collapse to a single strip and report the length it
    /// really reached, not the search bound it was handed.
    #[test]
    fn auto_width_uses_one_strip_and_reports_real_width() {
        let parts = vec![
            rect_part("small", 40.0, 30.0, 3),
            rect_part("bar", 100.0, 20.0, 2),
            rect_part("big", 60.0, 60.0, 1),
        ];
        // sheet_width is deliberately far too small to hold this set; auto
        // width must ignore it rather than spilling onto extra sheets.
        let cfg = NestConfig {
            sheet_width: 80.0,
            auto_width: true,
            sheet_height: 200.0,
            spacing: 2.0,
            margin: 5.0,
            time_limit_ms: 500,
            population: 8,
            ..NestConfig::default()
        };
        let sol = run_nest(&parts, &cfg, Hooks::default()).unwrap();

        assert_eq!(sol.stats.placed, 6);
        assert_eq!(sol.stats.sheets_used, 1, "auto width must never spill");
        assert!(
            sol.stats.used_width > 0.0 && sol.stats.used_width < 600.0,
            "used_width {} should be the reached length, not the bound",
            sol.stats.used_width
        );
        // The sheet to render is the reached length plus one margin.
        assert!((sol.sheet_width - (sol.stats.used_width + cfg.margin)).abs() < 1e-9);
        assert_eq!(sol.sheet_height, 200.0);
        // Auto width has no operator-chosen sheet, so the plain utilization
        // must be measured against the strip actually consumed rather than
        // against the (much wider) search bound.
        assert_eq!(sol.stats.utilization, sol.stats.strip_utilization);
        assert!(
            sol.stats.utilization > 0.1,
            "utilization {} collapsed towards the search bound",
            sol.stats.utilization
        );
        // Every placement has to sit inside the reported width.
        for p in &sol.placements {
            assert!(
                p.dx <= sol.stats.used_width + 1e-6,
                "placement dx {} beyond used_width {}",
                p.dx,
                sol.stats.used_width
            );
        }
    }

    /// The same parts under auto width must never need more stock than a
    /// bounded sheet that forces them to spill onto a second sheet.
    #[test]
    fn auto_width_beats_a_sheet_that_forces_a_spill() {
        let parts = vec![
            // 12*2000 + 4*3600 = 38400mm2 against 140*190 = 26600mm2 of
            // usable sheet, so a 150x200 sheet cannot hold this in one.
            rect_part("bar", 100.0, 20.0, 12),
            rect_part("big", 60.0, 60.0, 4),
        ];
        let base = NestConfig {
            sheet_height: 200.0,
            spacing: 2.0,
            margin: 5.0,
            time_limit_ms: 30_000,
            stale_generations: 60,
            population: 8,
            seed: Some(11),
            ..NestConfig::default()
        };
        let bounded = run_nest(
            &parts,
            &NestConfig {
                sheet_width: 150.0,
                ..base.clone()
            },
            Hooks::default(),
        )
        .unwrap();
        let auto = run_nest(
            &parts,
            &NestConfig {
                sheet_width: 150.0,
                auto_width: true,
                ..base.clone()
            },
            Hooks::default(),
        )
        .unwrap();

        assert!(bounded.stats.sheets_used > 1, "fixture should spill");
        assert_eq!(bounded.sheet_width, 150.0, "bounded keeps the sheet");
        assert_eq!(auto.stats.sheets_used, 1);
        assert!(
            auto.stats.used_width <= bounded.stats.used_width + 1e-6,
            "auto {} should not consume more than bounded {}",
            auto.stats.used_width,
            bounded.stats.used_width
        );
    }

    #[test]
    fn mixed_rects_fit_one_sheet() {
        let parts = vec![
            rect_part("small", 40.0, 30.0, 3),
            rect_part("bar", 100.0, 20.0, 2),
            rect_part("big", 60.0, 60.0, 1),
        ];
        let cfg = NestConfig {
            sheet_width: 200.0,
            sheet_height: 200.0,
            spacing: 2.0,
            margin: 5.0,
            time_limit_ms: 500,
            population: 8,
            ..NestConfig::default()
        };
        let calls = Arc::new(AtomicU32::new(0));
        let calls2 = calls.clone();
        let hooks = Hooks::new().on_progress(move |p| {
            assert!(p.generation >= 1);
            calls2.fetch_add(1, Ordering::Relaxed);
        });
        let sol = run_nest(&parts, &cfg, hooks).unwrap();

        assert_eq!(sol.stats.total, 6);
        assert_eq!(sol.stats.placed, 6);
        assert_eq!(sol.placements.len(), 6);
        assert_eq!(sol.stats.sheets_used, 1);
        assert!(
            sol.stats.utilization > 0.2,
            "utilization {}",
            sol.stats.utilization
        );
        assert!(sol.stats.strip_utilization >= sol.stats.utilization - 1e-9);
        // no-overlap is enforced by run_nest (it errors on violations).
        assert!(sol.stats.generations >= 1);
        assert_eq!(calls.load(Ordering::Relaxed), sol.stats.generations);
        assert!(matches!(
            sol.stats.stop_reason,
            StopReason::TimeLimit | StopReason::Stale
        ));
        // placements sorted by sheet then part id.
        for w in sol.placements.windows(2) {
            assert!((w[0].sheet, w[0].part_id) <= (w[1].sheet, w[1].part_id));
        }
        // part_id is the index into `parts`.
        for p in &sol.placements {
            assert_eq!(parts[p.part_id].name, p.part_name);
        }
    }

    #[test]
    fn rotation_needed_to_fit_bars() {
        // 100x10 bars cannot fit a 45-wide sheet unrotated; at 90 degrees
        // four of them fit side by side on one sheet.
        let parts = vec![rect_part("bar", 100.0, 10.0, 4)];
        let cfg = NestConfig {
            sheet_width: 45.0,
            sheet_height: 120.0,
            spacing: 0.5,
            margin: 1.0,
            rotation_mode: RotationMode::Orthogonal,
            time_limit_ms: 500,
            population: 6,
            ..NestConfig::default()
        };
        let sol = run_nest(&parts, &cfg, Hooks::default()).unwrap();
        assert_eq!(sol.stats.placed, 4);
        assert_eq!(sol.stats.sheets_used, 1, "must fit on a single sheet");
        for p in &sol.placements {
            let r = p.rotation_deg.rem_euclid(180.0);
            assert!(
                (r - 90.0).abs() < 1e-9,
                "expected a 90/270 rotation, got {}",
                p.rotation_deg
            );
        }
    }

    #[test]
    fn respects_time_limit() {
        let parts = vec![
            rect_part("a", 30.0, 20.0, 8),
            rect_part("b", 25.0, 25.0, 8),
            rect_part("c", 50.0, 10.0, 6),
        ];
        let cfg = NestConfig {
            sheet_width: 300.0,
            sheet_height: 300.0,
            spacing: 2.0,
            margin: 5.0,
            time_limit_ms: 300,
            population: 10,
            ..NestConfig::default()
        };
        let t0 = Instant::now();
        let sol = run_nest(&parts, &cfg, Hooks::default()).unwrap();
        let wall = t0.elapsed().as_millis() as u64;
        assert!(
            wall < cfg.time_limit_ms * 3 + 500,
            "run_nest took {wall}ms for a {}ms budget",
            cfg.time_limit_ms
        );
        assert_eq!(sol.stats.placed, 22);
        assert!(sol.stats.elapsed_ms <= wall + 1);
        assert_eq!(sol.stats.stop_reason, StopReason::TimeLimit);
    }

    #[test]
    fn oversized_part_yields_warning_not_error() {
        let parts = vec![rect_part("huge", 500.0, 500.0, 1)];
        let cfg = NestConfig {
            sheet_width: 100.0,
            sheet_height: 100.0,
            spacing: 2.0,
            margin: 5.0,
            time_limit_ms: 200,
            population: 4,
            ..NestConfig::default()
        };
        let sol = run_nest(&parts, &cfg, Hooks::default()).unwrap();
        assert_eq!(sol.stats.placed, 0);
        assert_eq!(sol.stats.total, 1);
        assert_eq!(sol.stats.sheets_used, 0);
        assert_eq!(sol.stats.utilization, 0.0);
        assert!(
            sol.warnings
                .iter()
                .any(|w| w.contains("could not be placed"))
        );
    }

    #[test]
    fn zero_instances_returns_empty_solution() {
        let parts = vec![rect_part("none", 10.0, 10.0, 0)];
        let cfg = NestConfig {
            time_limit_ms: 100,
            ..NestConfig::default()
        };
        let sol = run_nest(&parts, &cfg, Hooks::default()).unwrap();
        assert!(sol.placements.is_empty());
        assert_eq!(sol.stats.total, 0);
        assert_eq!(sol.stats.sheets_used, 0);
        assert_eq!(sol.stats.stop_reason, StopReason::Empty);
        assert_eq!(sol.sheet_width, cfg.sheet_width);
    }

    /// A seeded run that ends on the stale limit reproduces its layout
    /// exactly; a different seed is allowed to differ.
    #[test]
    fn seed_makes_the_layout_reproducible() {
        let parts = vec![
            rect_part("a", 30.0, 20.0, 5),
            rect_part("b", 25.0, 25.0, 4),
            rect_part("c", 50.0, 10.0, 3),
        ];
        let cfg = NestConfig {
            sheet_width: 200.0,
            sheet_height: 200.0,
            spacing: 2.0,
            margin: 5.0,
            time_limit_ms: 60_000,
            stale_generations: 15,
            population: 8,
            seed: Some(7),
            ..NestConfig::default()
        };
        let a = run_nest(&parts, &cfg, Hooks::default()).unwrap();
        let b = run_nest(&parts, &cfg, Hooks::default()).unwrap();
        assert_eq!(a.stats.stop_reason, StopReason::Stale);
        assert_eq!(a.stats.generations, b.stats.generations);
        assert_eq!(a.placements.len(), b.placements.len());
        for (x, y) in a.placements.iter().zip(&b.placements) {
            assert_eq!(x.part_id, y.part_id);
            assert_eq!(x.instance, y.instance);
            assert_eq!(x.sheet, y.sheet);
            assert_eq!(x.rotation_deg, y.rotation_deg);
            assert_eq!(x.dx, y.dx);
            assert_eq!(x.dy, y.dy);
        }
    }

    /// `should_stop` ends the run after the current generation with the
    /// best layout so far.
    #[test]
    fn should_stop_cancels_with_a_valid_partial_result() {
        let parts = vec![rect_part("a", 30.0, 20.0, 6), rect_part("b", 25.0, 25.0, 6)];
        let cfg = NestConfig {
            sheet_width: 200.0,
            sheet_height: 200.0,
            time_limit_ms: 60_000,
            stale_generations: 10_000,
            population: 6,
            ..NestConfig::default()
        };
        let seen = Arc::new(AtomicU32::new(0));
        let seen2 = seen.clone();
        let hooks = Hooks::new()
            .on_progress(move |p| seen2.store(p.generation, Ordering::Relaxed))
            .should_stop(move || seen.load(Ordering::Relaxed) >= 3);
        let sol = run_nest(&parts, &cfg, hooks).unwrap();
        assert_eq!(sol.stats.stop_reason, StopReason::Cancelled);
        assert_eq!(sol.stats.generations, 3);
        assert_eq!(sol.stats.placed, 12);
    }
}
