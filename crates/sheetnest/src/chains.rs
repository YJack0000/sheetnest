//! Cut chains: the exact geometry to cut for one part, after micro-joints.
//! Shared by the DXF writer and the SVG renderer.

use crate::model::{NestConfig, Part, Seg};
use crate::tabs;

/// Chain endpoints closer than this are considered coincident.
pub(crate) const CLOSE_TOL: f64 = 1e-6;

/// |sweep| at or above this is a full circle and needs special handling
/// (DXF: start == end is ambiguous; SVG: a single A command cannot do it).
pub(crate) const FULL_CIRCLE_DEG: f64 = 359.999;

/// Cut chains for one part in LOCAL coordinates. With tabs disabled this
/// is simply the contours' segs (outer first, then holes). With tabs
/// enabled each contour is split into open chains around the tab gaps, so
/// every instance of the same part gets identical tab positions.
pub(crate) fn part_chains(part: &Part, cfg: &NestConfig) -> Vec<Vec<Seg>> {
    if cfg.tabs.enabled {
        tabs::apply_tabs(&part.outer, &part.holes, &cfg.tabs).0
    } else {
        let mut v: Vec<Vec<Seg>> = Vec::with_capacity(1 + part.holes.len());
        v.push(part.outer.segs.clone());
        for h in &part.holes {
            v.push(h.segs.clone());
        }
        v
    }
}

/// A chain is closed when its last endpoint meets its first start point.
pub(crate) fn chain_is_closed(chain: &[Seg]) -> bool {
    match (chain.first(), chain.last()) {
        (Some(f), Some(l)) => f.start().dist(&l.end()) < CLOSE_TOL,
        _ => false,
    }
}
