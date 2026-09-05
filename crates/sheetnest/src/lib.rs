//! 2D nesting for sheet cutting: pack parts onto rectangular stock with as
//! little waste as possible.
//!
//! The optimizer is a no-fit-polygon (NFP) placer driven by a genetic
//! algorithm over placement order and rotation, the same family as SVGnest,
//! implemented natively on top of Clipper2. Arcs stay analytic all the way
//! to the output, so a laser or plasma cutter gets true `ARC` entities, and
//! optional micro-joints (tabs) keep cut parts from tipping into the head.
//!
//! All lengths are millimeters, all angles degrees, y is up.
//!
//! ```
//! use sheetnest::{Hooks, NestConfig, Part, Pt, nest};
//!
//! let plate = Part::from_polygon(
//!     "plate",
//!     4,
//!     &[Pt::new(0.0, 0.0), Pt::new(120.0, 0.0), Pt::new(120.0, 80.0), Pt::new(0.0, 80.0)],
//!     &[],
//! )?;
//! let cfg = NestConfig {
//!     sheet_width: 500.0,
//!     sheet_height: 300.0,
//!     spacing: 2.0,
//!     margin: 5.0,
//!     time_limit_ms: 500,
//!     seed: Some(1),
//!     ..NestConfig::default()
//! };
//! let solution = nest(&[plate], &cfg, Hooks::default())?;
//! assert_eq!(solution.stats.placed, 4);
//! for p in &solution.placements {
//!     println!("{} #{} on sheet {} at ({:.1}, {:.1}) rot {}°",
//!         p.part_name, p.instance, p.sheet, p.dx, p.dy, p.rotation_deg);
//! }
//! # Ok::<(), anyhow::Error>(())
//! ```
//!
//! With the `dxf` feature, [`dxf::parse_dxf`] reads parts from a drawing
//! and [`to_dxf`] writes the nested layout back out; with `svg`, [`to_svg`]
//! renders a sheet for preview. The `parallel` feature (on by default)
//! evaluates the population on all cores with rayon.

#[cfg(any(feature = "dxf", feature = "svg"))]
mod chains;
pub mod ga;
pub mod geom;
pub mod model;
pub mod place;
pub mod tabs;

#[cfg(feature = "dxf")]
pub mod dxf;
#[cfg(feature = "svg")]
pub mod svg;

#[cfg(all(test, any(feature = "dxf", feature = "svg")))]
pub(crate) mod test_util;

pub use ga::Hooks;
pub use model::{
    Contour, NestConfig, NestSolution, NestStats, Part, Placement, Progress, Pt, RotationMode, Seg,
    StopReason, TabConfig,
};

/// Nest `parts` onto sheets described by `cfg`.
///
/// Returns the best layout found within `cfg.time_limit_ms`, or earlier if
/// `cfg.stale_generations` pass without improvement or `hooks.should_stop`
/// asks for it. Parts that cannot fit on an empty sheet are left out and
/// reported in `warnings`. An error means the input was unusable, not that
/// the layout is poor.
pub fn nest(parts: &[Part], cfg: &NestConfig, hooks: Hooks) -> anyhow::Result<NestSolution> {
    ga::run_nest(parts, cfg, hooks)
}

/// Serialize a solution to a DXF file for the cutter: every sheet side by
/// side along +X, sheet outlines on layer `SHEET`, cut geometry on `CUT`.
#[cfg(feature = "dxf")]
pub fn to_dxf(
    solution: &NestSolution,
    parts: &[Part],
    cfg: &NestConfig,
) -> anyhow::Result<Vec<u8>> {
    dxf::write_dxf(
        parts,
        &solution.placements,
        &solution.render_config(cfg),
        solution.stats.sheets_used,
    )
}

/// Render one sheet of a solution as an SVG document.
#[cfg(feature = "svg")]
pub fn to_svg(solution: &NestSolution, parts: &[Part], cfg: &NestConfig, sheet: usize) -> String {
    svg::sheet_svg(
        parts,
        &solution.placements,
        &solution.render_config(cfg),
        sheet,
    )
}

/// Render every sheet of a solution, stacked vertically in one SVG.
#[cfg(feature = "svg")]
pub fn to_svg_all(solution: &NestSolution, parts: &[Part], cfg: &NestConfig) -> String {
    let rcfg = solution.render_config(cfg);
    let sheets: Vec<String> = (0..solution.stats.sheets_used)
        .map(|s| svg::sheet_svg(parts, &solution.placements, &rcfg, s))
        .collect();
    svg::stack_sheets(&sheets, &rcfg)
}
