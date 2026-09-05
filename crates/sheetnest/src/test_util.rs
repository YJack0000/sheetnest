//! Fixtures shared by the renderer tests.

use crate::model::{Contour, NestConfig, Part, Placement, Pt, Seg, TabConfig};

pub(crate) fn square_contour(x0: f64, y0: f64, side: f64) -> Contour {
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

/// A 100x100 "washer": square outer with a full-circle hole r=20.
pub(crate) fn washer() -> Part {
    let outer = square_contour(0.0, 0.0, 100.0);
    let hole = Contour {
        segs: vec![Seg::Arc {
            c: Pt::new(50.0, 50.0),
            r: 20.0,
            start_deg: 0.0,
            sweep_deg: 360.0,
        }],
    };
    mk_part("washer", outer, vec![hole])
}

/// A rounded slot: two lines + two half arcs, 120 x 40.
pub(crate) fn slot() -> Part {
    let outer = Contour {
        segs: vec![
            Seg::Line {
                a: Pt::new(20.0, 0.0),
                b: Pt::new(100.0, 0.0),
            },
            Seg::Arc {
                c: Pt::new(100.0, 20.0),
                r: 20.0,
                start_deg: -90.0,
                sweep_deg: 180.0,
            },
            Seg::Line {
                a: Pt::new(100.0, 40.0),
                b: Pt::new(20.0, 40.0),
            },
            Seg::Arc {
                c: Pt::new(20.0, 20.0),
                r: 20.0,
                start_deg: 90.0,
                sweep_deg: 180.0,
            },
        ],
    };
    mk_part("slot", outer, vec![])
}

/// Build a part WITHOUT normalization so tests control the exact geometry.
pub(crate) fn mk_part(name: &str, outer: Contour, holes: Vec<Contour>) -> Part {
    let poly = outer.polyline(0.25);
    let gross = outer.signed_area(0.25).abs();
    let net = gross - holes.iter().map(|h| h.signed_area(0.25).abs()).sum::<f64>();
    Part {
        name: name.to_string(),
        quantity: 1,
        outer,
        holes,
        outer_poly: poly,
        gross_area: gross,
        net_area: net,
    }
}

pub(crate) fn place(
    part_id: usize,
    name: &str,
    sheet: usize,
    rot: f64,
    dx: f64,
    dy: f64,
) -> Placement {
    Placement {
        part_id,
        part_name: name.to_string(),
        instance: 0,
        sheet,
        rotation_deg: rot,
        dx,
        dy,
    }
}

pub(crate) fn base_cfg() -> NestConfig {
    NestConfig {
        sheet_width: 400.0,
        sheet_height: 300.0,
        ..Default::default()
    }
}

pub(crate) fn tab_cfg() -> TabConfig {
    TabConfig {
        enabled: true,
        width: 0.5,
        max_spacing: 1000.0,
        min_per_contour: 2,
        corner_clearance: 3.0,
        min_hole_size: 10.0,
    }
}
