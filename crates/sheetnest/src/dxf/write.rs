//! DXF output: nested layout -> one drawing for the machine.
//!
//! Sheets are laid out left to right along +X with a 50mm gap. Layers:
//!   "SHEET" — one closed LWPOLYLINE rectangle per sheet (color 8, gray).
//!   "CUT"   — part geometry: LINE and ARC entities (color 7).
//! Arcs are written as true ARC entities (CCW; CW sweeps are emitted with
//! start/end swapped). Tabs (when enabled) are applied per part in local
//! coordinates BEFORE transforming, so every instance of the same part gets
//! identical tab positions.

use std::collections::HashMap;

use dxf::entities::{Arc as DxfArc, Entity, EntityType, Line as DxfLine, LwPolyline};
use dxf::enums::{AcadVersion, Units};
use dxf::tables::Layer;
use dxf::{Color, Drawing, LwPolylineVertex, Point};

use crate::chains::{FULL_CIRCLE_DEG, part_chains};
use crate::model::{NestConfig, Part, Placement, Seg};

/// Gap between consecutive sheets in the combined DXF, mm.
pub const SHEET_GAP_MM: f64 = 50.0;

pub const LAYER_SHEET: &str = "SHEET";
pub const LAYER_CUT: &str = "CUT";

/// X offset of sheet `i` in the combined DXF.
pub fn sheet_offset(i: usize, cfg: &NestConfig) -> f64 {
    i as f64 * (cfg.sheet_width + SHEET_GAP_MM)
}

/// Serialize a nested layout to a DXF file (bytes of an R2000 ASCII DXF).
/// `parts` is indexed by `Placement::part_id`; placements whose part is
/// missing are skipped. `cfg.sheet_width` is the width the sheets are
/// drawn at — under `auto_width` pass [`crate::NestSolution::render_config`].
pub fn write_dxf(
    parts: &[Part],
    placements: &[Placement],
    cfg: &NestConfig,
    sheets_used: usize,
) -> anyhow::Result<Vec<u8>> {
    let mut dwg = Drawing::new();
    // LWPOLYLINE needs >= R14 and $INSUNITS needs >= R2000.
    dwg.header.version = AcadVersion::R2000;
    dwg.header.default_drawing_units = Units::Millimeters; // $INSUNITS = 4

    dwg.add_layer(Layer {
        name: LAYER_SHEET.to_string(),
        color: Color::from_index(8),
        ..Default::default()
    });
    dwg.add_layer(Layer {
        name: LAYER_CUT.to_string(),
        color: Color::from_index(7),
        ..Default::default()
    });

    // Sheet outlines.
    for i in 0..sheets_used {
        let x0 = sheet_offset(i, cfg);
        let x1 = x0 + cfg.sheet_width;
        let y1 = cfg.sheet_height;
        let mut lw = LwPolyline {
            vertices: [(x0, 0.0), (x1, 0.0), (x1, y1), (x0, y1)]
                .iter()
                .map(|(x, y)| LwPolylineVertex {
                    x: *x,
                    y: *y,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        lw.set_is_closed(true);
        let mut e = Entity::new(EntityType::LwPolyline(lw));
        e.common.layer = LAYER_SHEET.to_string();
        dwg.add_entity(e);
    }

    // Tabs are computed once per distinct part, in local coordinates, so
    // every instance of the same part gets identical tab positions.
    let mut cache: HashMap<usize, Vec<Vec<Seg>>> = HashMap::new();

    for pl in placements {
        let Some(part) = parts.get(pl.part_id) else {
            continue;
        };
        let chains = cache
            .entry(pl.part_id)
            .or_insert_with(|| part_chains(part, cfg));
        let off = sheet_offset(pl.sheet, cfg);
        for chain in chains.iter() {
            for s in chain {
                let t = s.transformed(pl.rotation_deg, pl.dx + off, pl.dy);
                let mut e = Entity::new(seg_entity(&t));
                e.common.layer = LAYER_CUT.to_string();
                dwg.add_entity(e);
            }
        }
    }

    let mut buf: Vec<u8> = Vec::new();
    dwg.save(&mut buf)
        .map_err(|e| anyhow::anyhow!("failed to serialize DXF: {e}"))?;
    Ok(buf)
}

fn norm360(a: f64) -> f64 {
    let mut v = a % 360.0;
    if v < 0.0 {
        v += 360.0;
    }
    // guard against -0.0 / 360.0 landing exactly on the boundary
    if v >= 360.0 { 0.0 } else { v }
}

/// One `Seg` -> one DXF entity. DXF ARCs are always CCW from `start_angle`
/// to `end_angle`, so a CW sweep is emitted with the two swapped.
fn seg_entity(s: &Seg) -> EntityType {
    match s {
        Seg::Line { a, b } => EntityType::Line(DxfLine {
            p1: Point::new(a.x, a.y, 0.0),
            p2: Point::new(b.x, b.y, 0.0),
            ..Default::default()
        }),
        Seg::Arc {
            c,
            r,
            start_deg,
            sweep_deg,
        } => {
            let (raw_start, raw_end) = if *sweep_deg >= 0.0 {
                (*start_deg, start_deg + sweep_deg)
            } else {
                // CW: swap so the CCW arc from start to end traces the
                // same geometry.
                (start_deg + sweep_deg, *start_deg)
            };
            let (start_angle, end_angle) = if sweep_deg.abs() >= FULL_CIRCLE_DEG {
                let s0 = norm360(raw_start);
                (s0, s0 + 360.0)
            } else {
                (norm360(raw_start), norm360(raw_end))
            };
            EntityType::Arc(DxfArc {
                center: Point::new(c.x, c.y, 0.0),
                radius: *r,
                start_angle,
                end_angle,
                ..Default::default()
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Pt, TabConfig};
    use crate::tabs;
    use crate::test_util::*;

    fn load(bytes: &[u8]) -> Drawing {
        let mut cur = std::io::Cursor::new(bytes.to_vec());
        Drawing::load(&mut cur).expect("DXF should parse back")
    }

    fn entities_on<'a>(d: &'a Drawing, layer: &str) -> Vec<&'a Entity> {
        d.entities().filter(|e| e.common.layer == layer).collect()
    }

    fn cut_length(d: &Drawing) -> f64 {
        entities_on(d, LAYER_CUT)
            .iter()
            .map(|e| match &e.specific {
                EntityType::Line(l) => {
                    let dx = l.p2.x - l.p1.x;
                    let dy = l.p2.y - l.p1.y;
                    (dx * dx + dy * dy).sqrt()
                }
                EntityType::Arc(a) => {
                    let mut sw = a.end_angle - a.start_angle;
                    while sw <= 0.0 {
                        sw += 360.0;
                    }
                    a.radius * sw.to_radians()
                }
                _ => 0.0,
            })
            .sum()
    }

    #[test]
    fn dxf_roundtrips_with_layers_and_arcs() {
        let parts = vec![washer(), slot()];
        let placements = vec![
            place(0, "washer", 0, 0.0, 10.0, 10.0),
            place(0, "washer", 1, 90.0, 200.0, 20.0),
            place(1, "slot", 0, 0.0, 150.0, 120.0),
        ];
        let cfg = base_cfg();
        let bytes = write_dxf(&parts, &placements, &cfg, 2).expect("write_dxf");
        let d = load(&bytes);

        // Header units are millimeters.
        assert_eq!(d.header.default_drawing_units, Units::Millimeters);

        // Both layers present with the documented colors.
        let sheet_layer = d.layers().find(|l| l.name == LAYER_SHEET).expect("SHEET");
        assert_eq!(sheet_layer.color.index(), Some(8));
        let cut_layer = d.layers().find(|l| l.name == LAYER_CUT).expect("CUT");
        assert_eq!(cut_layer.color.index(), Some(7));

        // One closed LWPOLYLINE per sheet.
        let sheets = entities_on(&d, LAYER_SHEET);
        assert_eq!(sheets.len(), 2);
        for e in &sheets {
            match &e.specific {
                EntityType::LwPolyline(lw) => {
                    assert!(lw.is_closed());
                    assert_eq!(lw.vertices.len(), 4);
                }
                other => panic!("expected LwPolyline, got {other:?}"),
            }
        }
        // Second sheet is offset along +X.
        let xs: Vec<f64> = sheets
            .iter()
            .filter_map(|e| match &e.specific {
                EntityType::LwPolyline(lw) => Some(lw.vertices[0].x),
                _ => None,
            })
            .collect();
        assert!(xs.contains(&0.0));
        assert!(
            xs.iter()
                .any(|x| (*x - (400.0 + SHEET_GAP_MM)).abs() < 1e-9)
        );

        // CUT geometry exists and includes true ARCs.
        let cut = entities_on(&d, LAYER_CUT);
        assert!(!cut.is_empty(), "no CUT entities");
        let arcs = cut
            .iter()
            .filter(|e| matches!(e.specific, EntityType::Arc(_)))
            .count();
        assert!(arcs > 0, "expected ARC entities for the curved parts");

        // 2 washers (4 lines + 1 circle-arc) + 1 slot (2 lines + 2 arcs).
        assert_eq!(cut.len(), 2 * 5 + 4);

        // Total cut length matches the sum of placed perimeters.
        let expect = 2.0 * (parts[0].outer.perimeter() + parts[0].holes[0].perimeter())
            + parts[1].outer.perimeter();
        assert!(
            (cut_length(&d) - expect).abs() < 1e-6,
            "cut length {} != {}",
            cut_length(&d),
            expect
        );
    }

    #[test]
    fn tabs_add_entities_and_shorten_the_cut() {
        let parts = vec![washer()];
        let placements = vec![place(0, "washer", 0, 0.0, 10.0, 10.0)];

        let mut plain = base_cfg();
        plain.tabs = TabConfig::default(); // disabled
        let d_plain = load(&write_dxf(&parts, &placements, &plain, 1).unwrap());

        let mut tabbed = base_cfg();
        tabbed.tabs = tab_cfg();
        let d_tabbed = load(&write_dxf(&parts, &placements, &tabbed, 1).unwrap());

        let n_plain = entities_on(&d_plain, LAYER_CUT).len();
        let n_tabbed = entities_on(&d_tabbed, LAYER_CUT).len();
        assert!(
            n_tabbed > n_plain,
            "tabs should split segs: {n_plain} -> {n_tabbed}"
        );

        // outer square: 2 tabs; r=20 hole (bbox 40 >= min_hole_size 10): 1 tab.
        let (_, tab_count) = tabs::apply_tabs(&parts[0].outer, &parts[0].holes, &tabbed.tabs);
        assert_eq!(tab_count, 3);

        let shrink = cut_length(&d_plain) - cut_length(&d_tabbed);
        assert!(
            (shrink - tab_count as f64 * tabbed.tabs.width).abs() < 1e-6,
            "cut shrank by {shrink}, expected {}",
            tab_count as f64 * tabbed.tabs.width
        );
    }

    #[test]
    fn cw_arc_is_emitted_with_swapped_angles() {
        // CW quarter arc from 90deg down to 0deg.
        let s = Seg::Arc {
            c: Pt::new(0.0, 0.0),
            r: 5.0,
            start_deg: 90.0,
            sweep_deg: -90.0,
        };
        match seg_entity(&s) {
            EntityType::Arc(a) => {
                assert!((a.start_angle - 0.0).abs() < 1e-9, "{:?}", a.start_angle);
                assert!((a.end_angle - 90.0).abs() < 1e-9, "{:?}", a.end_angle);
                assert!((a.radius - 5.0).abs() < 1e-9);
            }
            other => panic!("expected Arc, got {other:?}"),
        }
    }

    #[test]
    fn full_circle_arc_keeps_its_sweep() {
        let s = Seg::Arc {
            c: Pt::new(1.0, 2.0),
            r: 3.0,
            start_deg: 0.0,
            sweep_deg: 360.0,
        };
        match seg_entity(&s) {
            EntityType::Arc(a) => {
                assert!((a.end_angle - a.start_angle - 360.0).abs() < 1e-9);
            }
            other => panic!("expected Arc, got {other:?}"),
        }
    }

    #[test]
    fn empty_input_still_produces_valid_output() {
        let cfg = base_cfg();
        let bytes = write_dxf(&[], &[], &cfg, 0).expect("write_dxf");
        let d = load(&bytes);
        assert_eq!(entities_on(&d, LAYER_CUT).len(), 0);
    }

    #[test]
    fn placement_with_unknown_part_is_skipped() {
        let parts = vec![washer()];
        let placements = vec![place(7, "ghost", 0, 0.0, 10.0, 10.0)];
        let d = load(&write_dxf(&parts, &placements, &base_cfg(), 1).unwrap());
        assert_eq!(entities_on(&d, LAYER_CUT).len(), 0);
    }
}
