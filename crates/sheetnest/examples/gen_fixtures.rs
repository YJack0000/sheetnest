//! Generate demo DXF parts for manual testing of the nesting engine.
//! Usage: cargo run --example gen_fixtures [out_dir]   (default: fixtures/)

use dxf::entities::*;
use dxf::enums::AcadVersion;
use dxf::{Drawing, LwPolylineVertex, Point};

fn lw(vertices: &[(f64, f64, f64)]) -> Entity {
    let mut p = LwPolyline {
        flags: 1, // closed
        ..Default::default()
    };
    for &(x, y, bulge) in vertices {
        p.vertices.push(LwPolylineVertex {
            x,
            y,
            bulge,
            ..Default::default()
        });
    }
    Entity::new(EntityType::LwPolyline(p))
}

fn circle(cx: f64, cy: f64, r: f64) -> Entity {
    Entity::new(EntityType::Circle(Circle {
        center: Point::new(cx, cy, 0.0),
        radius: r,
        ..Default::default()
    }))
}

fn save(name: &str, out_dir: &str, entities: Vec<Entity>) {
    let mut d = Drawing::new();
    d.header.version = AcadVersion::R2013;
    for e in entities {
        d.add_entity(e);
    }
    let path = format!("{out_dir}/{name}");
    d.save_file(&path).expect("save dxf");
    println!("wrote {path}");
}

fn main() {
    let out_dir = std::env::args().nth(1).unwrap_or_else(|| "fixtures".into());
    std::fs::create_dir_all(&out_dir).expect("mkdir");

    // L-bracket 120x80, leg width 30, two r=5 holes
    save(
        "bracket_l.dxf",
        &out_dir,
        vec![
            lw(&[
                (0.0, 0.0, 0.0),
                (120.0, 0.0, 0.0),
                (120.0, 30.0, 0.0),
                (30.0, 30.0, 0.0),
                (30.0, 80.0, 0.0),
                (0.0, 80.0, 0.0),
            ]),
            circle(105.0, 15.0, 5.0),
            circle(15.0, 65.0, 5.0),
        ],
    );

    // 150x100 plate, r=10 rounded corners (bulge = tan(22.5 deg)), four r=6 holes
    let b = (22.5f64).to_radians().tan();
    save(
        "plate_rounded.dxf",
        &out_dir,
        vec![
            lw(&[
                (10.0, 0.0, 0.0),
                (140.0, 0.0, b),
                (150.0, 10.0, 0.0),
                (150.0, 90.0, b),
                (140.0, 100.0, 0.0),
                (10.0, 100.0, b),
                (0.0, 90.0, 0.0),
                (0.0, 10.0, b),
            ]),
            circle(20.0, 20.0, 6.0),
            circle(130.0, 20.0, 6.0),
            circle(130.0, 80.0, 6.0),
            circle(20.0, 80.0, 6.0),
        ],
    );

    // disc r=40 with r=10 center hole
    save(
        "disc.dxf",
        &out_dir,
        vec![circle(40.0, 40.0, 40.0), circle(40.0, 40.0, 10.0)],
    );

    // long strip 200x20
    save(
        "strip.dxf",
        &out_dir,
        vec![lw(&[
            (0.0, 0.0, 0.0),
            (200.0, 0.0, 0.0),
            (200.0, 20.0, 0.0),
            (0.0, 20.0, 0.0),
        ])],
    );

    // triangular gusset 90x90 with r=8 hole
    save(
        "gusset.dxf",
        &out_dir,
        vec![
            lw(&[(0.0, 0.0, 0.0), (90.0, 0.0, 0.0), (0.0, 90.0, 0.0)]),
            circle(25.0, 25.0, 8.0),
        ],
    );
}
