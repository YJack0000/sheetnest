//! Probe NFP results for pinhole artifacts (tiny hole rings).
use sheetnest::model::{NestConfig, ring_signed_area};
use sheetnest::{dxf, geom, place};

fn main() {
    let cfg = NestConfig::default();
    let mut parts = Vec::new();
    for f in ["bracket_l", "disc", "gusset", "plate_rounded", "strip"] {
        let bytes = std::fs::read(format!("fixtures/{f}.dxf")).unwrap();
        let mut parsed = dxf::parse_dxf(&bytes, f, 1, cfg.curve_tolerance).unwrap();
        parts.append(&mut parsed.parts);
    }
    let prepped = place::prep_parts(&parts, &cfg);
    let angles: Vec<f64> = (0..24).map(|i| i as f64 * 15.0).collect();
    let mut pinholes = 0;
    for a in &prepped {
        for b in &prepped {
            for &ra in &angles {
                for &rb in &angles {
                    let ring_a = geom::rotate_ring(&a.raw, ra);
                    let ring_b = geom::rotate_ring(&b.inflated, rb);
                    let nfp = geom::nfp(&ring_a, &ring_b);
                    for ring in geom::paths_to_rings(&nfp) {
                        let area = ring_signed_area(&ring);
                        if area < 0.0 && area.abs() < 25.0 {
                            pinholes += 1;
                            if pinholes <= 10 {
                                println!(
                                    "PINHOLE part{}@{ra} vs part{}@{rb}: hole area {:.4} mm^2",
                                    a.id,
                                    b.id,
                                    area.abs()
                                );
                            }
                        }
                    }
                }
            }
        }
    }
    println!("total pinholes across {} NFPs: {pinholes}", 5 * 5 * 24 * 24);
}
