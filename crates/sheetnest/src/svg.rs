//! SVG preview rendering (`svg` feature).
//!
//! One `<svg>` per sheet, viewBox = sheet rect with a small padding,
//! y-axis flipped (SVG y-down vs DXF y-up), parts as `<path>` using line and
//! arc (A) commands, sheet outline as a rect, part fill translucent so
//! overlaps would be visually obvious. Paths carry `data-part="name"` and
//! `data-instance` attributes for UIs that want to highlight parts.

use std::collections::HashMap;
use std::fmt::Write as _;

use crate::chains::{FULL_CIRCLE_DEG, chain_is_closed, part_chains};
use crate::model::{NestConfig, Part, Placement, Pt, Seg};

/// Padding around the sheet rect in the SVG viewBox, mm.
pub const SVG_PAD: f64 = 10.0;

/// Render one sheet to an SVG string. `sheet` is the sheet index; only
/// placements with that sheet index are drawn, in sheet-local coordinates
/// (no inter-sheet offset). `cfg.sheet_width` is the width the sheet is
/// drawn at — under `auto_width` pass [`crate::NestSolution::render_config`].
pub fn sheet_svg(
    parts: &[Part],
    placements: &[Placement],
    cfg: &NestConfig,
    sheet: usize,
) -> String {
    let w = cfg.sheet_width;
    let h = cfg.sheet_height;

    let mut out = String::new();
    let _ = write!(
        out,
        concat!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="{} {} {} {}""#,
            r#" width="100%" height="100%" data-sheet="{}" data-sheet-width="{}""#,
            r#" data-sheet-height="{}">"#
        ),
        num(-SVG_PAD),
        num(-SVG_PAD),
        num(w + 2.0 * SVG_PAD),
        num(h + 2.0 * SVG_PAD),
        sheet,
        num(w),
        num(h),
    );

    // Sheet outline (already in flipped space: the rect is symmetric).
    let _ = write!(
        out,
        concat!(
            r##"<rect x="0" y="0" width="{}" height="{}" fill="none" stroke="#999999""##,
            r#" stroke-width="0.8" vector-effect="non-scaling-stroke"/>"#
        ),
        num(w),
        num(h),
    );

    let mut cache: HashMap<usize, Vec<Vec<Seg>>> = HashMap::new();
    for pl in placements {
        if pl.sheet != sheet {
            continue;
        }
        let Some(part) = parts.get(pl.part_id) else {
            continue;
        };
        let chains = cache
            .entry(pl.part_id)
            .or_insert_with(|| part_chains(part, cfg));
        let name = escape_attr(&pl.part_name);
        for chain in chains.iter() {
            if chain.is_empty() {
                continue;
            }
            let placed: Vec<Seg> = chain
                .iter()
                .map(|s| s.transformed(pl.rotation_deg, pl.dx, pl.dy))
                .collect();
            let closed = chain_is_closed(&placed);
            let d = chain_path_data(&placed, h);
            if d.is_empty() {
                continue;
            }
            let fill = if closed {
                "rgba(56,132,255,0.25)"
            } else {
                "none"
            };
            let _ = write!(
                out,
                concat!(
                    r##"<path d="{}" fill="{}" fill-rule="evenodd" stroke="#3884ff""##,
                    r#" stroke-width="0.6" vector-effect="non-scaling-stroke""#,
                    r#" data-part="{}" data-instance="{}"/>"#
                ),
                d, fill, name, pl.instance,
            );
        }
    }

    out.push_str("</svg>");
    out
}

/// Stack per-sheet SVGs (from [`sheet_svg`]) vertically into a single
/// document. Each sheet becomes a nested `<svg>` tile whose inner viewBox
/// keeps coordinates untouched.
pub fn stack_sheets(svgs: &[String], cfg: &NestConfig) -> String {
    let tile_w = cfg.sheet_width + 2.0 * SVG_PAD;
    let tile_h = cfg.sheet_height + 2.0 * SVG_PAD;
    let gap = 20.0;
    let n = svgs.len().max(1);
    let total_h = n as f64 * tile_h + (n - 1) as f64 * gap;

    let mut out = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {} {}" width="100%">"#,
        num(tile_w),
        num(total_h)
    );
    for (i, s) in svgs.iter().enumerate() {
        let y = i as f64 * (tile_h + gap);
        // Drop the outer 100% sizing, then re-host the sheet as a tile.
        let inner = s.replacen(r#" width="100%" height="100%""#, "", 1);
        out.push_str(&inner.replacen(
            "<svg ",
            &format!(
                r#"<svg x="0" y="{}" width="{}" height="{}" "#,
                num(y),
                num(tile_w),
                num(tile_h)
            ),
            1,
        ));
    }
    out.push_str("</svg>");
    out
}

/// Format a length for SVG/attribute output: 4 decimals, trailing zeros
/// stripped, no "-0".
fn num(v: f64) -> String {
    if !v.is_finite() {
        return "0".to_string();
    }
    let mut s = format!("{v:.4}");
    if s.contains('.') {
        s = s.trim_end_matches('0').trim_end_matches('.').to_string();
    }
    if s == "-0" || s.is_empty() {
        s = "0".to_string();
    }
    s
}

/// DXF y-up -> SVG y-down, done on the coordinates themselves so the
/// emitted path data is directly readable.
fn flip(y: f64, sheet_height: f64) -> f64 {
    sheet_height - y
}

/// `d` attribute for one chain, already in sheet-local coordinates.
/// Closed chains get a trailing `Z`.
pub(crate) fn chain_path_data(chain: &[Seg], sheet_height: f64) -> String {
    let mut d = String::new();
    if chain.is_empty() {
        return d;
    }
    let s0 = chain[0].start();
    let _ = write!(d, "M {} {}", num(s0.x), num(flip(s0.y, sheet_height)));
    for s in chain {
        match s {
            Seg::Line { b, .. } => {
                let _ = write!(d, " L {} {}", num(b.x), num(flip(b.y, sheet_height)));
            }
            Seg::Arc {
                c,
                r,
                start_deg,
                sweep_deg,
            } => arc_commands(*c, *r, *start_deg, *sweep_deg, sheet_height, &mut d),
        }
    }
    if chain_is_closed(chain) {
        d.push_str(" Z");
    }
    d
}

/// Append the `A` command(s) for one arc.
///
/// The sweep flag is inverted relative to the raw math direction because
/// the y-flip mirrors the plane: an arc that runs CCW in DXF space runs in
/// SVG's *decreasing*-angle direction once y has been flipped, and SVG
/// calls that `sweep-flag = 0`.
///
/// A full circle cannot be expressed by a single `A` (start == end), so it
/// is emitted as two half sweeps.
fn arc_commands(c: Pt, r: f64, start_deg: f64, sweep_deg: f64, h: f64, out: &mut String) {
    let pieces: Vec<(f64, f64)> = if sweep_deg.abs() >= FULL_CIRCLE_DEG {
        let half = sweep_deg / 2.0;
        vec![(start_deg, half), (start_deg + half, half)]
    } else {
        vec![(start_deg, sweep_deg)]
    };

    for (s0, sw) in pieces {
        let end_rad = (s0 + sw).to_radians();
        let ex = c.x + r * end_rad.cos();
        let ey = c.y + r * end_rad.sin();
        let large_arc = if sw.abs() > 180.0 { 1 } else { 0 };
        let sweep_flag = if sw > 0.0 { 0 } else { 1 };
        let _ = write!(
            out,
            " A {} {} 0 {} {} {} {}",
            num(r),
            num(r),
            large_arc,
            sweep_flag,
            num(ex),
            num(flip(ey, h)),
        );
    }
}

fn escape_attr(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::*;

    #[test]
    fn stack_sheets_tiles_vertically() {
        let cfg = NestConfig {
            sheet_width: 100.0,
            sheet_height: 50.0,
            ..Default::default()
        };
        let sheet = |i: usize| {
            format!(
                r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="-10 -10 120 70" width="100%" height="100%" data-sheet="{i}"><rect/></svg>"#
            )
        };
        let out = stack_sheets(&[sheet(0), sheet(1)], &cfg);
        // tiles: 2 * (50+20) + 20 gap = 160 total height, tile width 120
        assert!(
            out.starts_with(r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 120 160""#)
        );
        assert_eq!(out.matches("<svg x=\"0\" y=").count(), 2);
        assert!(out.contains(r#"y="90""#)); // second tile offset = 70 + 20
        // inner 100% sizing must be gone so tiles honor x/y/width/height
        assert!(!out.contains(r#"width="100%" height="100%""#));
        assert_eq!(out.matches("</svg>").count(), 3);
    }

    /// Pull the `d="..."` payloads out of an SVG string.
    fn path_data(svg: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut rest = svg;
        while let Some(i) = rest.find("<path ") {
            rest = &rest[i..];
            let Some(j) = rest.find("d=\"") else { break };
            let after = &rest[j + 3..];
            let Some(k) = after.find('"') else { break };
            out.push(after[..k].to_string());
            rest = &after[k..];
        }
        out
    }

    /// Every (x, y) the pen actually visits in a `d` string.
    fn path_points(d: &str) -> Vec<(f64, f64)> {
        let toks: Vec<&str> = d.split_whitespace().collect();
        let mut pts = Vec::new();
        let mut i = 0;
        while i < toks.len() {
            match toks[i] {
                "M" | "L" => {
                    let x: f64 = toks[i + 1].parse().unwrap();
                    let y: f64 = toks[i + 2].parse().unwrap();
                    pts.push((x, y));
                    i += 3;
                }
                "A" => {
                    // rx ry rot large sweep x y
                    let x: f64 = toks[i + 6].parse().unwrap();
                    let y: f64 = toks[i + 7].parse().unwrap();
                    pts.push((x, y));
                    i += 8;
                }
                "Z" => i += 1,
                t => panic!("unexpected path token {t:?} in {d:?}"),
            }
        }
        pts
    }

    #[test]
    fn svg_has_one_path_per_chain_inside_the_viewbox() {
        let parts = vec![washer(), slot()];
        let placements = vec![
            place(0, "washer", 0, 0.0, 20.0, 20.0),
            place(1, "slot", 0, 0.0, 200.0, 150.0),
            place(0, "washer", 1, 0.0, 20.0, 20.0), // other sheet, ignored
        ];
        let cfg = base_cfg();
        let svg = sheet_svg(&parts, &placements, &cfg, 0);

        assert!(svg.starts_with("<svg "), "{}", &svg[..40.min(svg.len())]);
        assert!(svg.ends_with("</svg>"));
        assert!(
            svg.contains(r#"viewBox="-10 -10 420 320""#),
            "bad viewBox in {svg}"
        );
        assert!(svg.contains("<rect "), "missing sheet rect");
        assert!(svg.contains(r#"data-part="washer""#));
        assert!(svg.contains(r#"data-part="slot""#));

        // washer = outer + hole = 2 chains; slot = 1 chain.
        let ds = path_data(&svg);
        assert_eq!(ds.len(), 3, "expected one <path> per chain");

        // All pen positions inside the padded viewBox.
        for d in &ds {
            for (x, y) in path_points(d) {
                assert!(
                    (-SVG_PAD..=cfg.sheet_width + SVG_PAD).contains(&x)
                        && (-SVG_PAD..=cfg.sheet_height + SVG_PAD).contains(&y),
                    "point ({x}, {y}) outside viewBox"
                );
            }
        }

        // Closed chains carry the translucent fill and a Z.
        assert!(ds.iter().all(|d| d.ends_with(" Z")));
        assert!(svg.contains(r#"fill="rgba(56,132,255,0.25)""#));
    }

    #[test]
    fn svg_arc_endpoint_matches_seg_end_after_yflip() {
        let h = 300.0;
        for (start, sweep) in [
            (0.0, 90.0),
            (10.0, -90.0),
            (200.0, 270.0),
            (-45.0, -200.0),
            (30.0, 360.0),
        ] {
            let seg = Seg::Arc {
                c: Pt::new(123.0, 45.0),
                r: 17.5,
                start_deg: start,
                sweep_deg: sweep,
            };
            let d = chain_path_data(std::slice::from_ref(&seg), h);
            let pts = path_points(&d);
            let (lx, ly) = *pts.last().unwrap();
            let want = seg.end();
            assert!(
                (lx - want.x).abs() < 1e-3 && (ly - flip(want.y, h)).abs() < 1e-3,
                "start {start} sweep {sweep}: A endpoint ({lx}, {ly}) != \
                 Seg::end mapped ({}, {}) — d = {d}",
                want.x,
                flip(want.y, h)
            );
            // First point is the start, also y-flipped.
            let (fx, fy) = pts[0];
            let s = seg.start();
            assert!((fx - s.x).abs() < 1e-3 && (fy - flip(s.y, h)).abs() < 1e-3);
        }
    }

    #[test]
    fn svg_arc_flags_account_for_the_yflip() {
        let h = 100.0;
        // CCW in DXF space -> SVG decreasing-angle direction -> flag 0.
        let ccw = Seg::Arc {
            c: Pt::new(0.0, 0.0),
            r: 10.0,
            start_deg: 0.0,
            sweep_deg: 90.0,
        };
        let d = chain_path_data(std::slice::from_ref(&ccw), h);
        let toks: Vec<&str> = d.split_whitespace().collect();
        let a = toks.iter().position(|t| *t == "A").unwrap();
        assert_eq!(toks[a + 4], "0", "large-arc for 90deg"); // rx ry rot large
        assert_eq!(toks[a + 5], "0", "sweep flag for CCW after y-flip");

        // CW -> flag 1; > 180deg -> large-arc 1.
        let cw = Seg::Arc {
            c: Pt::new(0.0, 0.0),
            r: 10.0,
            start_deg: 0.0,
            sweep_deg: -270.0,
        };
        let d = chain_path_data(std::slice::from_ref(&cw), h);
        let toks: Vec<&str> = d.split_whitespace().collect();
        let a = toks.iter().position(|t| *t == "A").unwrap();
        assert_eq!(toks[a + 4], "1", "large-arc for 270deg");
        assert_eq!(toks[a + 5], "1", "sweep flag for CW after y-flip");
    }

    #[test]
    fn svg_full_circle_uses_two_arc_commands() {
        let circle = Seg::Arc {
            c: Pt::new(50.0, 50.0),
            r: 20.0,
            start_deg: 0.0,
            sweep_deg: 360.0,
        };
        let d = chain_path_data(std::slice::from_ref(&circle), 300.0);
        assert_eq!(d.matches(" A ").count(), 2, "{d}");
        assert!(d.ends_with(" Z"), "a full circle chain is closed: {d}");
    }

    #[test]
    fn svg_open_tab_chains_are_not_filled() {
        let parts = vec![washer()];
        let placements = vec![place(0, "washer", 0, 0.0, 20.0, 20.0)];
        let mut cfg = base_cfg();
        cfg.tabs = tab_cfg();
        let svg = sheet_svg(&parts, &placements, &cfg, 0);

        let ds = path_data(&svg);
        // 2 outer chains + 1 hole chain
        assert_eq!(ds.len(), 3);
        assert!(ds.iter().all(|d| !d.ends_with(" Z")), "tab chains are open");
        assert_eq!(svg.matches(r#"fill="none""#).count(), 1 + ds.len());
    }

    #[test]
    fn svg_yflip_puts_the_origin_at_the_bottom() {
        let parts = vec![mk_part("sq", square_contour(0.0, 0.0, 10.0), vec![])];
        let placements = vec![place(0, "sq", 0, 0.0, 0.0, 0.0)];
        let cfg = base_cfg(); // 400 x 300
        let svg = sheet_svg(&parts, &placements, &cfg, 0);
        let d = &path_data(&svg)[0];
        // local (0,0) -> svg (0, 300); local (0,10) -> svg (0, 290)
        assert!(d.starts_with("M 0 300"), "{d}");
        assert!(d.contains("L 0 290"), "{d}");
    }

    #[test]
    fn part_name_is_escaped() {
        let parts = vec![mk_part("a&b", square_contour(0.0, 0.0, 10.0), vec![])];
        let placements = vec![place(0, "a<b>&\"c\"", 0, 0.0, 5.0, 5.0)];
        let svg = sheet_svg(&parts, &placements, &base_cfg(), 0);
        assert!(
            svg.contains(r#"data-part="a&lt;b&gt;&amp;&quot;c&quot;""#),
            "{svg}"
        );
    }

    #[test]
    fn empty_input_still_produces_valid_output() {
        let cfg = base_cfg();
        let svg = sheet_svg(&[], &[], &cfg, 0);
        assert!(svg.contains("<rect "));
        assert!(path_data(&svg).is_empty());
    }
}
